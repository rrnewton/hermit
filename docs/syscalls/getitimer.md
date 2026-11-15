---
schema: syscall-pages/v1
syscall: getitimer
nr_x86_64: 36
linux_determinism: nondeterministic
target_policy: model
tracking_status: todo
cross_host_variant: false
related:
  - https://github.com/rrnewton/hermit/issues/604
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+getitimer&body=Discussion+for+the+%60getitimer%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fgetitimer.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60getitimer%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 239ca43d3092fe64884782ff3166baf34059e41c
  detcore_class: determinized
  dispatch: "typed"
  refused: false
  refused_only_when_fail_closed: false
  subscribed_run: true
  subscribed_record_replay: true
  tests_naming: 2
---

## Summary

`getitimer(which, value)` reads one of the process's three interval timers: the time left before it next fires, and its reload interval. Detcore handles it in `handle_getitimer` (`detcore/src/syscalls/signal.rs:327`). It answers from its own logical timer state: `ITIMER_REAL` comes from the virtual-time alarm that `alarm` and `setitimer` arm, and `ITIMER_VIRTUAL`/`ITIMER_PROF` read as disarmed, because Detcore never lets a guest arm them.

Manual reference: Linux man-pages getitimer(2).

## Linux semantics

A process has three interval timers, shared by all its threads:

- `ITIMER_REAL` counts down in real (wall-clock) time and sends `SIGALRM`. `alarm(2)` uses the same timer.
- `ITIMER_VIRTUAL` counts down only while the process runs in user mode and sends `SIGVTALRM`.
- `ITIMER_PROF` counts down while the process runs in user or kernel mode and sends `SIGPROF`.

`getitimer` writes a `struct itimerval` with `it_value` (time to the next expiry, zero if disarmed) and `it_interval` (the reload period, zero for a one-shot timer). Values are in microseconds, truncated from the kernel's nanosecond count. Once a one-shot timer has expired it is disarmed and reads zero, even while its signal is still pending because the process blocks it. In the short window where the kernel timer is still active but its deadline has passed, Linux reports 1 microsecond rather than zero (`itimer_get_remtime` in `kernel/time/itimer.c`); user space cannot reliably hit that window.

The kernel checks `which` first and copies the result out afterwards, so a bad `which` gives `EINVAL` even with a bad pointer. A child created by `fork` starts with all three timers disarmed. Timers survive `execve`.

### Flags and options

- F1: `ITIMER_REAL`: report the real-time timer that `alarm` and `setitimer(ITIMER_REAL)` arm; zeros when disarmed
- F2: `ITIMER_VIRTUAL`: report the user-CPU-time timer
- F3: `ITIMER_PROF`: report the user-plus-system-CPU-time timer
- F4: any other `which`: `EINVAL`, checked before `value`
- F5: `value` NULL or unwritable: `EFAULT`
- F6: a periodic timer: `it_interval` reports the reload period, and `it_value` the time to the next expiry
- F7: expiry: an expired one-shot `ITIMER_REAL` reads zeros even while its `SIGALRM` is still pending; a timer still active past its deadline reads 1 microsecond, not zero
- F8: timers belong to the process: every thread reads the same values, a `fork` child reads all three disarmed, and an `execve` keeps them

## Determinism analysis

`linux_determinism: nondeterministic`, kept from the legacy classification. `ITIMER_REAL`'s remaining time is measured against host wall-clock time, so two runs that read it at the same program point see different values. `ITIMER_VIRTUAL` and `ITIMER_PROF` count CPU time, which also varies with host load and frequency. Only the error cases (F4, F5), a disarmed timer, and the interval field repeat on their own.

## Hermit design

Target policy: `model`. Detcore keeps logical timer state and answers from it. When threads are sequentialized (`config.sequentialize_threads`, set at `hermit-cli/src/bin/hermit/run.rs:3667`), the handler at `detcore/src/syscalls/signal.rs:327` does this:

- `ITIMER_REAL` asks the global scheduler for this process's alarm (`alarm_remaining`, `detcore/src/tool_global.rs:3977`, handled at `detcore/src/tool_global.rs:1640`). On ptrace, `itimer_snapshot` (`detcore/src/scheduler/parked.rs:582`) reads the alarm keyed by process id and computes the remaining time against global virtual time (`detcore/src/scheduler.rs:5823`), with the stored interval. This covers F1 and F6. The same timer is armed by `alarm` and by `setitimer(ITIMER_REAL)` (`register_alarm`, `detcore/src/scheduler.rs:5772`), so the two calls see each other's state as on Linux.
- `ITIMER_VIRTUAL` and `ITIMER_PROF` return zeros (`detcore/src/syscalls/signal.rs:342`). That is consistent because `setitimer` refuses those timers with `ENOSYS` (`detcore/src/syscalls/signal.rs:304`), so no guest can arm them under Hermit. For F2 and F3, Hermit's answer is "disarmed", which is what Linux says for a timer nobody armed.
- Any other `which` gives `EINVAL` (`detcore/src/syscalls/signal.rs:345`), before the pointer check, then NULL `value` gives `EFAULT` (`detcore/src/syscalls/signal.rs:347`). The result is written with microsecond truncation (`logical_time_to_timeval`, `detcore/src/syscalls/signal.rs:207`), matching Linux, and a write fault on `value` returns that error (`detcore/src/syscalls/signal.rs:352`). This covers F4 and F5.
- Timers are keyed by Detcore process id, so threads share them and a `fork` child, which has a new process id, starts disarmed. Timers are dropped when a process logically exits (`detcore/src/scheduler.rs:2352`), and exec does not touch them. That matches F8, though no test checks it.
- Without sequentialization, the call goes to the kernel unchanged (`detcore/src/syscalls/signal.rs:337`).

Why `model`: a host `getitimer` would leak wall-clock and CPU time. `refuse` would break programs that read their alarm, and a fixed `emulate` answer would disagree with the timer Detcore actually fires.

Possible gap, F7: on ptrace the remaining time saturates at zero (`detcore/src/scheduler.rs:5823`), so a timer still stored past its deadline would read as disarmed rather than as 1 microsecond. The KVM timer state applies Linux's minimum (`active_remaining`, `detcore/src/scheduler/real_timer.rs:112`). Whether a ptrace guest can ever read the timer in that state was not measured; on Linux the window is a race that programs do not depend on.

Target (owner decision 13): once `setitimer` arms `ITIMER_VIRTUAL` and `ITIMER_PROF` on virtual CPU time (see the `setitimer` body; the clock names are those defined in the `clock_gettime` body), `getitimer` reports what is left of them. For `ITIMER_VIRTUAL` that is the deadline minus the process's virtual user CPU time. For `ITIMER_PROF` it is the deadline minus virtual user plus system CPU time. Both read the counters behind `process_cpu_time` (`detcore/src/tool_local.rs:2146`), and `it_interval` reports the stored reload period (F2, F3, F6). Today both read zeros unconditionally (`detcore/src/syscalls/signal.rs:342`). `ITIMER_REAL` stays as it is. The kernel order, `which` `EINVAL` before the `EFAULT` copy-out, already matches (`detcore/src/syscalls/signal.rs:345`, `detcore/src/syscalls/signal.rs:347`), as owner decision 6 requires.

`linux_determinism` stays `nondeterministic`, because that describes the kernel. Under Hermit the values are a function of virtual time and repeat.

## Recommended tests

- T1 (covers F1, F4): `ITIMER_REAL` reads zeros before arming, a nonzero value no larger than the armed value with zero interval after `setitimer({0,0},{5,0})`, and zeros after disarming. `syscall(SYS_getitimer, 99, &v)` gives `EINVAL`.
  Existing candidate: `tests/c/getitimer_determinism_probe.c`, run by `getitimer_tracks_logical_alarm_state` at `hermit-cli/tests/getitimer_determinism.rs:13` with `--backend=ptrace run --strict --verify`, and by the cell `c-programs/getitimer-determinism-probe` (`tests/e2e/manifests/c-programs.yaml:1989`).
- T2 (covers F2, F3): `getitimer(ITIMER_VIRTUAL)` and `getitimer(ITIMER_PROF)` read zeros before arming. After `setitimer` arms each for 1 second with a 100 ms interval, and the process spins for a fixed amount of work, they report the armed value minus the virtual CPU time used and the 100 ms interval. Both values must be identical across `hermit run --verify` runs, and a sleep in `nanosleep` must not reduce them. At this SHA Hermit's `setitimer` returns `ENOSYS` for both timers, and `getitimer` reads zeros.
- T3 (covers F4, F5): with a raw `syscall(SYS_getitimer, ...)`: `which = 99` with `value` NULL gives `EINVAL`, not `EFAULT`; `ITIMER_REAL` with `value` NULL, and with `value` on a read-only page, gives `EFAULT`.
- T4 (covers F6): arm `setitimer(ITIMER_REAL, {{0, 250000}, {1, 0}})` with a counting handler, let it fire twice, and read `getitimer` after each delivery. `it_interval` must read `{0, 250000}` every time, and `it_value` must be identical across `hermit run --verify` runs.
- T5 (covers F1): `alarm(3)` followed by `getitimer(ITIMER_REAL)` reads a value of at most 3 seconds, and `setitimer` followed by `alarm(0)` returns the setitimer's remaining time rounded to seconds, showing the shared timer.
- T6 (covers F7): block `SIGALRM`, arm a one-shot `ITIMER_REAL` of 1 second, sleep 2 seconds, then call `getitimer` before unblocking. It must read zeros, and the handler must run once after unblocking, identically across `hermit run --verify` runs.
- T7 (covers F8): arm `ITIMER_REAL`, then read it from a second thread (same values), from a `fork` child (zeros), and after `execve` of a helper that reads it at once (still armed).

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant B of https://github.com/rrnewton/hermit/issues/604: linux_determinism nondeterministic (from **Classification:**); target_policy from legacy NEEDS-HANDLER; an agent chooses model, emulate or refuse; code at this SHA: determinized, dispatch typed.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy investigate -> model (ITIMER_REAL from Detcore's per-process virtual-time alarm; VIRTUAL/PROF read as disarmed because setitimer refuses them); linux_determinism stays nondeterministic; 8 flag behaviours and 7 tests listed; a timer still stored past its deadline would read 0 on ptrace where Linux reads 1 microsecond (F7, not measured); no coverage token yet, so tracking_status stays todo
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d615 (owner decisions 6-15): applied 13 (ITIMER_VIRTUAL and ITIMER_PROF report remaining virtual user and user+system CPU time instead of fixed zeros) and 6 (which EINVAL before EFAULT already matches); target_policy stays model; no new F/T ids (T2 rewritten to the target); no open questions existed

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `getitimer`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+getitimer&body=Discussion+for+the+%60getitimer%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fgetitimer.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60getitimer%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: getitimer" (https://github.com/rrnewton/hermit/issues/604), legacy variant B. The legacy note below describes the code as of 2026-07-24, before `handle_getitimer` answered from Detcore's logical timers; it is kept as history.

### Legacy tracking note (2026-07-24)

Filed by the complete syscall-classification audit. The implementation classifies `PASS-THRU` entries under fixed-container/stable-state assumptions and routes all other entries through the explicit unsupported policy until a deterministic handler is implemented.
