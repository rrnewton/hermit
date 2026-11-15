---
schema: syscall-pages/v1
syscall: timer_create
nr_x86_64: 222
linux_determinism: conditional
target_policy: model
tracking_status: todo
cross_host_variant: false
related: []
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+timer_create&body=Discussion+for+the+%60timer_create%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Ftimer_create.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60timer_create%60%2C+or+in+what+the+page+says%3F%0A
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

`timer_create(clockid, sevp, timerid)` creates a per-process POSIX interval timer on a clock and stores its id in `*timerid`; `sevp` says how an expiration is reported. Detcore emulates the whole timer family (`detcore/src/syscalls/time.rs:461`): it keeps its own per-process timer table with ids 0, 1, 2 in creation order, never creates a host timer, and arms timers on the virtual clock so the scheduler delivers the expiration signal. The clock id is ignored, and several notification forms are accepted but never deliver.

Manual reference: Linux man-pages timer_create(2), sigevent(7).

## Linux semantics

The kernel allocates a timer in the calling process (all threads share it) and returns a small integer id; on current kernels ids come from a per-process counter, so a fresh process usually gets 0, 1, 2 in creation order. The timer starts disarmed; `timer_settime` arms it. Timers are not inherited by `fork`, are deleted by a successful `execve`, and survive a failed one.

The clock decides what time the timer measures: wall-clock time (`CLOCK_REALTIME`, `CLOCK_TAI`), time since boot excluding or including suspend (`CLOCK_MONOTONIC`, `CLOCK_BOOTTIME`), or CPU time consumed (`CLOCK_PROCESS_CPUTIME_ID`, `CLOCK_THREAD_CPUTIME_ID`, and the negative ids returned by `clock_getcpuclockid`/`pthread_getcpuclockid`). The `*_ALARM` clocks also wake a suspended system and need `CAP_WAKE_ALARM`. An unknown clock id gives `EINVAL`; a known clock that cannot back a timer (`CLOCK_MONOTONIC_RAW`, the `*_COARSE` clocks, or an alarm clock without an RTC) gives `EOPNOTSUPP`.

`sevp` chooses the notification. NULL means `SIGEV_SIGNAL` with `SIGALRM` and `sigev_value` set to the timer id. `SIGEV_SIGNAL` queues `sigev_signo` to the process with `si_code` `SI_TIMER`, `si_timerid`, `si_overrun` and `si_value`; while one expiration signal is pending, further expirations only raise the overrun count. `SIGEV_THREAD_ID` sends the signal to the thread `sigev_notify_thread_id`, which must belong to the caller's thread group. `SIGEV_NONE` sends nothing. `SIGEV_THREAD` is a glibc feature built on `SIGEV_THREAD_ID` with a reserved real-time signal and a helper thread that runs the callback. A bad notify value or signal number gives `EINVAL`, a bad pointer `EFAULT`, and a failed allocation, including the per-user pending-signal accounting, `EAGAIN`.

### Flags and options

- F1: clock `CLOCK_REALTIME`, `CLOCK_MONOTONIC`, `CLOCK_BOOTTIME` or `CLOCK_TAI`: the timer measures that clock.
- F2: CPU-time clocks (`CLOCK_PROCESS_CPUTIME_ID`, `CLOCK_THREAD_CPUTIME_ID`, and the negative ids from `clock_getcpuclockid`/`pthread_getcpuclockid`): the timer measures CPU time consumed, not elapsed time.
- F3: `CLOCK_REALTIME_ALARM` and `CLOCK_BOOTTIME_ALARM`: `EPERM` without `CAP_WAKE_ALARM`, `EOPNOTSUPP` without an RTC.
- F4: unknown clock id: `EINVAL`; a known clock with no timer support (`CLOCK_MONOTONIC_RAW`, `CLOCK_REALTIME_COARSE`, `CLOCK_MONOTONIC_COARSE`): `EOPNOTSUPP`.
- F5: `sevp` NULL: notify with `SIGALRM`, and `sigev_value` is the timer id.
- F6: `SIGEV_NONE`: no notification; the timer still runs and `timer_gettime` reports it.
- F7: `SIGEV_SIGNAL` with a standard or real-time `sigev_signo`: process-directed signal with `SI_TIMER`, `si_timerid`, `si_overrun` and `si_value`; one pending signal per timer, extra expirations counted as overruns.
- F8: `SIGEV_THREAD_ID`: signal directed at `sigev_notify_thread_id`, which must be a thread of the caller's process, otherwise `EINVAL`.
- F9: `SIGEV_THREAD` through glibc: implemented in user space on top of F8 with a reserved real-time signal; the callback runs in a helper thread.
- F10: invalid `sigev_notify` or `sigev_signo` outside 1 to `SIGRTMAX`: `EINVAL`.
- F11: `timerid` or `sevp` pointing at bad memory: `EFAULT`; the timer is not left behind.
- F12: id allocation and lifetime: ids per process in creation order, shared by every thread, not inherited across `fork`, deleted by a successful `execve`, kept by a failed one.
- F13: allocation failure or pending-signal limit reached: `EAGAIN`.
- F14: raw `sigev_notify` of `SIGEV_THREAD` (2) reaching the syscall, as from a libc that does not translate it: the kernel accepts it and treats it like `SIGEV_SIGNAL`, with the same `sigev_signo` range check (`good_sigevent` in `kernel/time/posix-timers.c`).

## Determinism analysis

`linux_determinism: conditional`. Creating a timer has no timing in it: the result depends on the arguments, the caller's capabilities and the process's own timer history, which are guest state. Two host factors can change it. The pending-signal accounting behind `EAGAIN` is shared by every process of the same real user on the host, and id allocation is a kernel implementation detail that has changed between kernel versions. Everything nondeterministic about timers happens later, when they are armed and expire; see the `timer_settime` and `timer_gettime` bodies.

### Nondeterminism sources

- `EAGAIN` from host-wide per-user pending-signal accounting (F13).
- Kernel-version-dependent id allocation (F12).
- Capability and hardware differences for alarm clocks (F3).

## Hermit design

Target policy: `model`. Detcore owns the timer table and the clock it measures, and the deterministic scheduler owns expirations. The handler at `detcore/src/syscalls/time.rs:461` (dispatch arm `detcore/src/lib.rs:2785`, grouped under the comment at `detcore/src/lib.rs:2783-2784`) never forwards the call:

- The table is per process: `PosixTimers` (`detcore/src/tool_local.rs:184-190`) allocates ids 0, 1, 2 in creation order (`detcore/src/tool_local.rs:196-208`). A new thread shares the table and a new process gets an empty one (`detcore/src/lib.rs:1561-1564`). A successful exec clears it while keeping the id counter (`detcore/src/lib.rs:1760-1765`, `detcore/src/tool_local.rs:258-262`) and drops its scheduler deadlines (`detcore/src/scheduler.rs:2408`). This realizes F12.
- A NULL `timerid` gives `EFAULT` (`detcore/src/syscalls/time.rs:467`). A NULL `sevp` means `SIGALRM` (`detcore/src/syscalls/time.rs:484`, F5). `SIGEV_NONE` stores no signal (`detcore/src/syscalls/time.rs:472`, F6). A `sigev_signo` outside 1 to 64 gives `EINVAL` (`detcore/src/syscalls/time.rs:476-477`).
- Expirations are delivered by the scheduler: `timer_settime` registers the deadline (`detcore/src/tool_global.rs:3993`, `detcore/src/scheduler.rs:5801`), and when virtual time passes it, `fire_alarm` (`detcore/src/scheduler.rs:3077`) picks a thread and sends the signal.

Why `model` and not the alternatives: `passthrough` would let host timers fire at host times, which is the main source of nondeterminism in this family; `refuse` breaks CPython and other runtimes that arm watchdog timers (`tests/standalone/strict_timer_create.sh:10-13` records CPython aborting on `timer_create` under `--strict` before this handler existed).

What remains before the code fully realizes `model`:

- **The clock id is ignored (F1 to F4).** It is read only for the detlog line (`detcore/src/syscalls/time.rs:468`, `detcore/src/syscalls/time.rs:494`). Every timer measures the one virtual clock, which is also what `clock_gettime` returns for every clock (`detcore/src/syscalls/time.rs:210-217`). So unknown clocks, clocks without timer support and alarm clocks are all accepted where Linux returns `EINVAL`, `EOPNOTSUPP` or `EPERM`, and a CPU-time timer expires after elapsed virtual time, including time the process spends blocked.
- **Real-time signals never fire (F7, F9).** The signal is converted with `Signal::try_from(event.sigev_signo).ok()` (`detcore/src/syscalls/time.rs:479`), where `Signal` is `nix::sys::signal::Signal` (`detcore/src/syscalls/time.rs:12`). By this reading of the code that enum has no real-time variants, so a `sigev_signo` from 32 to 64 is stored as "no signal" and the timer behaves like `SIGEV_NONE`: `timer_settime` registers only timers that have a signal (`detcore/src/syscalls/time.rs:545`). glibc's `SIGEV_THREAD` uses a reserved real-time signal, so by the same reading its callback never runs. This is a hypothesis from the code, not a measurement; T4 confirms or refutes it. `tests/c/timer_create_determinism.c` arms a `SIGRTMIN` timer for 300 seconds and never waits for it, so it cannot see this.
- **The expiration signal does not carry timer siginfo (F5, F7).** `signal_guest` sends it with `pidfd_send_signal` and a NULL `siginfo` (`detcore/src/scheduler.rs:3308-3316`), so by this reading the handler sees an `SI_USER` signal with no `si_timerid`, `si_overrun` or `si_value`, and `sigev_value` is never stored. Overruns are not counted (`timer_getoverrun` always returns 0, `detcore/src/syscalls/time.rs:589-607`).
- **`SIGEV_THREAD_ID` is treated as process-directed (F8).** The comment at `detcore/src/syscalls/time.rs:473-474` says so; `sigev_notify_thread_id` is neither validated nor used.
- **Error differences (F10, F11, F13).** An unknown `sigev_notify` gives `ENOSYS` (`detcore/src/syscalls/time.rs:481`) where Linux gives `EINVAL`. The id is allocated before it is written back (`detcore/src/syscalls/time.rs:486-492`), so a bad non-NULL `timerid` leaves an orphan timer and consumes an id, where Linux deletes the timer. `EAGAIN` never happens; there is no limit on timers, which keeps the result independent of the host.

Target (owner decisions 13 and 6): each timer records its clock and measures it. The clock names are those defined in the `clock_gettime` body.

- `CLOCK_REALTIME` and `CLOCK_TAI` timers measure virtual realtime, and `CLOCK_MONOTONIC` and `CLOCK_BOOTTIME` timers measure virtual uptime. Absolute deadlines on those clocks are converted as the `timer_settime` body describes.
- `CLOCK_PROCESS_CPUTIME_ID` and the per-process ids from `clock_getcpuclockid` measure the target process's virtual CPU time (`process_cpu_time`, `detcore/src/tool_local.rs:2146`). `CLOCK_THREAD_CPUTIME_ID` and the per-thread ids from `pthread_getcpuclockid` measure the target thread's virtual CPU time (`thread_cpu_time`, `detcore/src/tool_local.rs:2161`). Such a timer does not advance while its target is blocked.
- `CLOCK_MONOTONIC_RAW`, the two coarse clocks and every dynamic FD clock id give `EOPNOTSUPP`, because those clock types have no timer operation. Unknown ids give `EINVAL`.
- The two alarm clocks give `EPERM` on every host. A container process has no `CAP_WAKE_ALARM` in the initial user namespace, and the host's RTC is not consulted.

Kernel order (`SYSCALL_DEFINE3(timer_create)` and `do_timer_create` in `kernel/time/posix-timers.c`):

1. Copying `sevp` in gives `EFAULT`.
2. `clockid_to_kclock` gives `EINVAL`.
3. A clock without `timer_create` gives `EOPNOTSUPP`.
4. `good_sigevent` gives `EINVAL`. It accepts `SIGEV_NONE`, `SIGEV_SIGNAL`, raw `SIGEV_THREAD` (F14), and `SIGEV_THREAD_ID` naming a thread of the caller's group, with `sigev_signo` from 1 to `SIGRTMAX`.
5. Copying the id out to `timerid` gives `EFAULT`, and the timer is removed.
6. The clock's own `timer_create` runs. For the alarm clocks it gives `EPERM`. For a CPU clock naming no live task, or a thread outside the group, it gives `EINVAL`.

Today the handler checks `timerid` for NULL before anything else (`detcore/src/syscalls/time.rs:467`). It returns `ENOSYS` instead of `EINVAL` for an unaccepted `sigev_notify`, and also for raw `SIGEV_THREAD`, which Linux accepts (`detcore/src/syscalls/time.rs:481`). It writes the id only after the timer is in the table (`detcore/src/syscalls/time.rs:486` to `detcore/src/syscalls/time.rs:492`), so an unmapped `timerid` leaves an orphan timer.

`linux_determinism` stays `conditional`. Hermit removes both host factors: ids come from its own counter, and no host accounting is consulted.

## Recommended tests

- T1 (covers F1, F6, F12): create a `SIGEV_SIGNAL` timer and a `SIGEV_NONE` timer on `CLOCK_MONOTONIC`, arm one periodically, fail an exec and check both survive, then exec for real and check `timer_gettime` gives `EINVAL` for both, under `hermit run --strict --verify` on ptrace.
  Existing candidate: `run_ptrace_exec_deletes_posix_timers_and_preserves_itimer` at `hermit-cli/tests/cli.rs:11505`, driving `hermit-cli/tests/fixtures/exec_posix_timers.c` through `hermit-cli/tests/common/exec_posix_timers.rs:29`. Unit tests: `ids_are_deterministic_and_sequential` at `detcore/src/tool_local.rs:869` and `post_exec_deletes_armed_disarmed_and_non_notifying_timers` at `detcore/src/tool_global.rs:6058`.
- T2 (covers F5, F7): a NULL-`sevp` timer delivers `SIGALRM`, and a `SIGEV_SIGNAL` timer delivers its standard signal; the handler, installed with `SA_SIGINFO`, checks `si_code == SI_TIMER`, `si_timerid` and `si_value`.
  Existing candidate for delivery only: `tests/bin/posix_timer_test.c` (cell `bin-c/posix-timer-test`, `tests/e2e/manifests/bin-c.yaml:4`, verify ci on ptrace), which uses an explicit `SIGALRM` and does not inspect `siginfo`. The `siginfo` checks are expected to fail at this SHA.
- T3 (covers F2, F3, F4): create timers on `CLOCK_PROCESS_CPUTIME_ID`, a `pthread_getcpuclockid` clock, `CLOCK_BOOTTIME_ALARM`, `CLOCK_MONOTONIC_RAW` and clock id 99; record each result against the Linux results listed above, and check that a CPU-time timer does not expire while the process sleeps.
- T4 (covers F7, F9): a `SIGEV_SIGNAL` timer with `SIGRTMIN + 1` and a glibc `SIGEV_THREAD` timer, each armed for 10 ms with a bounded wait; both must notify.
- T5 (covers F8): a `SIGEV_THREAD_ID` timer aimed at a second thread; the signal must arrive on that thread, and a tid outside the process must give `EINVAL`.
- T6 (covers F10, F11, F13): `sigev_notify` 99 and `sigev_signo` 0 and 65 give `EINVAL`; NULL and unmapped `timerid` and `sevp` give `EFAULT`, and after the unmapped-`timerid` failure the next `timer_create` returns the id the failed call would have used; create 10 000 timers and record whether `EAGAIN` ever appears.
- T7 (covers F12): a forked child sees none of the parent's timers (`timer_gettime` gives `EINVAL`), and a second thread can arm a timer the first thread created.
  Related existing unit tests: `exec_deletes_only_its_process_posix_timers` at `detcore/src/scheduler/timed_waiters.rs:708` and `only_successful_exec_cancels_posix_deadlines` at `detcore/src/tool_global.rs:6136`.
- T8 (covers F2, F4, F10, F14): with a raw `syscall(SYS_timer_create, ...)`:
  - `sigev_notify` 2 with `SIGUSR1` succeeds, and the timer delivers `SIGUSR1` when armed;
  - clock id 99 with an unmapped `timerid` gives `EINVAL`, and `CLOCK_MONOTONIC_RAW` with an invalid `sigev_notify` gives `EOPNOTSUPP`, showing the clock is checked first;
  - a `CLOCK_THREAD_CPUTIME_ID` timer armed for 10 ms in a thread that sleeps for 1 s of virtual time does not fire, and fires once the thread has spun for 10 ms of virtual CPU time.
  The results must be identical across `hermit run --verify` runs.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · created for v1: no tracking issue existed for this pinned Sysno; code at this SHA: determinized, dispatch typed.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy model (Detcore-owned per-process timer table on the virtual clock, scheduler-delivered signals); linux_determinism stays conditional; gaps: clock id ignored, real-time and SIGEV_THREAD timers never fire by code reading, no SI_TIMER siginfo; 13 flag behaviours and 7 tests listed; tracking_status stays todo with open questions
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d615 (owner decisions 6-15): applied 13 and 6 (timers record their clock: virtual realtime, uptime, process or thread CPU time; EOPNOTSUPP/EPERM/EINVAL per clock; kernel order sevp EFAULT, clock, sigevent EINVAL instead of ENOSYS, timerid EFAULT without an orphan timer); target_policy stays model; new F14, T8; removed the open question on honouring the clock id

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `timer_create`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+timer_create&body=Discussion+for+the+%60timer_create%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Ftimer_create.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60timer_create%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Created by the syscall-tracking v1 migration on 2026-10-03. No tracking issue existed for `timer_create`: the 2026-07-24 classification campaign filed issues only for syscalls in the old temporary pass-through block, and Detcore already handled this one.

The handler carries the tags `AUTONOMOUS-BOT-IMPLEMENTED` and `TODO-HUMAN-REVIEW(#869)` (`detcore/src/syscalls/time.rs:457-458`). That tag matches commit 5687ea1683 (2026-07-27, "Determinize timer signal delivery"), whose subject names https://github.com/rrnewton/hermit/pull/869. The header comment of `tests/c/timer_create_determinism.c:13-15` says Detcore does not deliver expiration signals; that is stale at this SHA for standard signals. `tests/standalone/strict_timer_create.sh` runs that file, `tests/bin/posix_timer_test.c` and a periodic `setitimer` guest under `--strict --verify`. `tests/c/timer_family_identity.c` (cell `c-programs/timer-family-identity`, `tests/e2e/manifests/c-programs.yaml:17628`) checks the firing order of a `SIGUSR1` timer against other timer families, but its verify cell has `ci: false`. The sibling bodies `timer_settime.md` and `timer_gettime.md` describe arming and reading.

### Open questions

- Real-time signals: should the timer path keep the raw signal number (for example as `i32`) so `SIGRTMIN`-based timers and glibc `SIGEV_THREAD` work, given that delivery goes through `signal_guest`, which also takes a `nix` `Signal` (`detcore/src/scheduler.rs:3303`)?
- Should expiration signals carry `SI_TIMER` `siginfo` (`si_timerid`, `si_overrun`, `si_value`), which needs a non-NULL `siginfo` in `pidfd_send_signal` and storing `sigev_value` at creation?
