---
schema: syscall-pages/v1
syscall: clock_gettime
nr_x86_64: 228
linux_determinism: nondeterministic
target_policy: model
tracking_status: todo
cross_host_variant: false
related: []
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+clock_gettime&body=Discussion+for+the+%60clock_gettime%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fclock_gettime.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60clock_gettime%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 239ca43d3092fe64884782ff3166baf34059e41c
  detcore_class: determinized
  dispatch: "typed"
  refused: false
  refused_only_when_fail_closed: false
  subscribed_run: true
  subscribed_record_replay: true
  tests_naming: 14
---

## Summary

`clock_gettime(clockid, tp)` reads the current value of a clock into a `timespec`. With virtual time on, the default, Detcore answers every call from its deterministic virtual clock (`detcore/src/syscalls/time.rs:205`): the configured epoch plus logical time, the same value whichever clock is named. With `--no-virtualize-time` the call reads the host clock through the record/replay path (`detcore/src/lib.rs:2461`).

Manual reference: Linux man-pages clock_gettime(2).

## Linux semantics

The call writes the current value of the named clock into `*tp`. Most clocks are served in the vDSO without entering the kernel; the kernel path handles the rest and every error. The kernel first checks the clock id, giving `EINVAL` for an unknown id, then reads the clock and copies the result out, giving `EFAULT` for a bad pointer. Each clock has its own origin and rate: wall time since the Unix epoch, time since boot with or without suspend, or CPU time consumed. Time namespaces add a per-namespace offset to the monotonic and boot-time clocks.

### Flags and options

- F1: `CLOCK_REALTIME` and `CLOCK_REALTIME_COARSE`: wall-clock time since the Unix epoch. It jumps when an administrator sets the clock and is slewed by NTP. The coarse variant returns the value at the last tick.
- F2: `CLOCK_MONOTONIC`, `CLOCK_MONOTONIC_COARSE` and `CLOCK_MONOTONIC_RAW`: time since an unspecified starting point, in practice boot. It never jumps backward and does not count suspend. `CLOCK_MONOTONIC_RAW` is not slewed by NTP.
- F3: `CLOCK_BOOTTIME`: like `CLOCK_MONOTONIC`, but includes time spent suspended.
- F4: `CLOCK_TAI`: `CLOCK_REALTIME` plus the kernel's TAI offset, which is 0 unless time-keeping software set it.
- F5: `CLOCK_REALTIME_ALARM` and `CLOCK_BOOTTIME_ALARM`: read like `CLOCK_REALTIME` and `CLOCK_BOOTTIME`. On a host without an RTC wake-up device they give `EINVAL`.
- F6: CPU-time clocks: `CLOCK_PROCESS_CPUTIME_ID`, `CLOCK_THREAD_CPUTIME_ID`, and per-process or per-thread ids from `clock_getcpuclockid` or `pthread_getcpuclockid`. They return CPU time consumed, and `EINVAL` for an id naming a process or thread that does not exist.
- F7: dynamic FD clocks, whose id is built from a descriptor for a clock device such as `/dev/ptp0`: the device's time, and an error for a descriptor that is not a clock device.
- F8: an unknown clock id, such as 10 or any id above `CLOCK_TAI`: `EINVAL`.
- F9: a NULL or otherwise bad `tp`: `EFAULT`, checked after the clock id.
- F10: inside a time namespace, the monotonic and boot-time clocks include that namespace's offset.

## Determinism analysis

`linux_determinism: nondeterministic`. Every read of a host clock returns a different value. The value depends on when the call runs, on the host's boot time and suspend history, on NTP adjustment, on the CPU time the host scheduler has given the caller, and on the time namespace. Only the error results F8 and F9 repeat exactly.

### Nondeterminism sources

- Host wall-clock time, and administrator or NTP adjustments to it
- Host boot time and suspend history, which set the monotonic and boot-time origins
- Host scheduling, which sets elapsed time between reads and the CPU time consumed
- Time-namespace offsets
- Whether the host has an RTC wake-up device, for the alarm clocks (F5)

## Hermit design

Target policy: `model`. With virtual time on, Detcore replaces every clock with one deterministic clock:

- `handle_clock_gettime` (`detcore/src/syscalls/time.rs:205`) calls `guest_clock_time` (`detcore/src/syscalls/time.rs:95`). That reads the scheduler's global logical time (`thread_observe_time`, `detcore/src/tool_global.rs:3791`), which already includes the configured epoch, and passes it through a monotonic floor shared by the whole process tree (`GuestClock::observe`, `detcore/src/tool_local.rs:1700`; type doc at `detcore/src/tool_local.rs:1645`). The result is written to `*tp`.
- Each clock read is charged a base cost of 10,000 ns of virtual time (`CLOCK_NS`, `detcore/src/syscall_time.rs:11`; `clock_gettime` at `detcore/src/syscall_time.rs:31`), scaled by the configured clock multiplier, so a busy loop of reads sees time advance.
- The epoch comes from `--epoch` or `HERMIT_EPOCH` (`detcore-model/src/config.rs:205`), with a library default of 2026-01-01T00:00:00Z (`detcore-model/src/config.rs:1290`). When `hermit run` is given no epoch, it takes one host wall-clock sample before the guest starts (`capture_default_epoch`, `hermit-cli/src/bin/hermit/run.rs:3026`), and both halves of `--verify` share that sample. Two separate runs without `--epoch` therefore start from different times. This is the one deliberate nondeterministic input; pass `--epoch` for runs that must match each other.
- A NULL `tp` gives `EFAULT` (`detcore/src/syscalls/time.rs:213`), and a bad non-NULL pointer fails in the memory write. Both match F9 for valid clock ids.
- libc's `clock_gettime` normally runs in the vDSO without a syscall. The cargo test `tod_clock_gettime` (`detcore/tests/time/mod.rs:268`) calls libc and sees virtual time, so on ptrace the vDSO path reaches this handler. That interception lives in reverie, outside this export.

With `--no-virtualize-time` (`detcore-model/src/config.rs:70`) the arm calls `passthrough` (`detcore/src/lib.rs:2465`), which goes through `record_or_replay_preserving_tool_errors` (`detcore/src/lib.rs:372`): the guest reads the host clock, a recording captures each value, and replay returns it. The comment at `detcore/src/lib.rs:2444` states that rule for all clock syscalls.

`passthrough` with virtual time off is an explicit opt-out, not the target. `refuse` is impossible, because nearly every program reads a clock.

What remains before the code fully realizes `model`: the handler ignores `clockid`. Every listed clock, and every unknown id, gets the same epoch-based value. In detail:

- F2, F3, F4: the monotonic and boot-time clocks read wall-clock-like values near the epoch instead of time since boot. POSIX allows any starting point for `CLOCK_MONOTONIC`, but a program that treats `CLOCK_BOOTTIME` as uptime sees decades. `CLOCK_TAI` equals `CLOCK_REALTIME`, which is also what Linux reports with a zero TAI offset.
- F5: the alarm clocks never give `EINVAL`, even where the host has no RTC wake-up device.
- F6: the CPU-time clocks return the same wall-clock-like value, not CPU time consumed, and an id for a process or thread that does not exist gets 0 instead of `EINVAL`. Differences between two reads still advance with virtual time, so elapsed-CPU-time measurements stay plausible.
- F7, F8: dynamic FD clock ids and unknown ids get 0 and a time value instead of `EINVAL`. With a NULL `tp` they get `EFAULT`, where Linux would report `EINVAL` first.
- F10: virtual time ignores host time-namespace offsets, which is the intended isolation.

Target (owner decision 13, with decision 6 for the order of checks): every clock id reads its own virtual clock, and every id Linux rejects gets `EINVAL`. The clock bodies use one family of virtual clocks, with these names:

- **virtual realtime**: today's value, epoch plus logical time (`guest_clock_time`, `detcore/src/syscalls/time.rs:95`). It serves `CLOCK_REALTIME`, `CLOCK_REALTIME_COARSE`, `CLOCK_REALTIME_ALARM`, and `CLOCK_TAI` with a TAI offset of 0, the offset `adjtimex` already reports (`detcore/src/syscalls/time.rs:80`).
- **virtual uptime**: `--sysinfo-uptime-offset` (default 120 s, `detcore-model/src/config.rs:695`) plus the logical time elapsed since the epoch. This is the uptime `sysinfo`, `/proc/uptime` and `times` already report (`detcore/src/syscalls/sysinfo.rs:51`, `detcore/src/syscalls/sysinfo.rs:67`, `detcore/src/syscalls/sysinfo.rs:96`). It equals virtual realtime minus the virtual boot instant that `/proc/stat` reports as `btime` (`detcore/src/syscalls/sysinfo.rs:116`). It serves `CLOCK_MONOTONIC`, `CLOCK_MONOTONIC_COARSE`, `CLOCK_MONOTONIC_RAW`, `CLOCK_BOOTTIME` and `CLOCK_BOOTTIME_ALARM`. Virtual time has no suspend, so `CLOCK_BOOTTIME` equals `CLOCK_MONOTONIC`.
- **virtual CPU time**: the per-thread and per-process logical CPU counters that `getrusage` and `times` already report (`thread_cpu_time`, `detcore/src/tool_local.rs:2161`; `process_cpu_time`, `detcore/src/tool_local.rs:2146`). It serves `CLOCK_THREAD_CPUTIME_ID`, `CLOCK_PROCESS_CPUTIME_ID`, and the encoded ids from `pthread_getcpuclockid` and `clock_getcpuclockid`. An encoded id naming no live guest task, or a thread outside the caller's thread group, gets `EINVAL` (kernel `lookup_task` in `kernel/time/posix-cpu-timers.c`).
- **every other id** gets `EINVAL`. That covers the unknown ids, 10 and every id from 12 up, and the dynamic FD clock ids (F7). A PTP device clock cannot be virtual, and `EINVAL` is Linux's answer for a descriptor that is not a clock device (`get_clock_desc` in `kernel/time/posix-clock.c`).
- The alarm clocks read their virtual clock on every host, whether or not the host has an RTC, so F5's `EINVAL` never reaches the guest.

Kernel order (`SYSCALL_DEFINE2(clock_gettime)` in `kernel/time/posix-timers.c`):

1. `clockid_to_kclock` rejects an unknown or FD-invalid id with `EINVAL`.
2. The clock's `clock_get_timespec` runs, which gives `EINVAL` for a CPU clock naming no task.
3. `put_timespec64` gives `EFAULT`.

So an invalid id with a NULL `tp` gets `EINVAL`, not `EFAULT`. Today the handler never reads `clockid` and checks `tp` first (`detcore/src/syscalls/time.rs:205`, `detcore/src/syscalls/time.rs:213`), so every id reads virtual realtime. `tod_clock_gettime` (`detcore/tests/time/mod.rs:268`) expects `CLOCK_MONOTONIC` within 100 ms of the epoch. Under the target that read starts at the uptime offset instead, so the test changes with the code.

`linux_determinism` is `nondeterministic` because the host clocks are; under Hermit with virtual time and a fixed epoch the call repeats exactly.

## Recommended tests

- T1 (covers F1, F2, F3): under `hermit run --verify` on ptrace, read `CLOCK_REALTIME`, `CLOCK_MONOTONIC`, `CLOCK_MONOTONIC_RAW` and `CLOCK_BOOTTIME` repeatedly; values must match between the two runs and never decrease.
  Existing candidates: `tests/c/clock_determinism.c` (cell `system-utils/clock-determinism`, `tests/e2e/manifests/system-utils.yaml:4`, verify ci on ptrace), which reads `CLOCK_REALTIME`, `CLOCK_MONOTONIC`, `CLOCK_PROCESS_CPUTIME_ID` and `CLOCK_BOOTTIME` (`tests/c/clock_determinism.c:119`). The cargo target `tests_time` (`detcore/Cargo.toml:51`) has `tod_clock_gettime` (`detcore/tests/time/mod.rs:268`), which checks that `CLOCK_MONOTONIC` starts within 100 ms of the epoch, and `rdtsc_agrees_with_clock_gettime` (`detcore/tests/time/mod.rs:501`).
- T2 (covers F1, F4, F5): read the coarse, `CLOCK_TAI` and alarm clock ids and record Hermit's values and errors next to the host's.
  Related existing test: `tests/rust/clock_gettime.rs`, run by `default_cargo_clock_gettime` (`hermit-cli/tests/hermit_modes.rs:897`), reads ten clock ids including the coarse and alarm clocks (`tests/rust/clock_gettime.rs:19`), with `CLOCK_TAI` commented out (`tests/rust/clock_gettime.rs:32`). It checks only that a second read is not earlier than the first.
- T3 (covers F6): read `CLOCK_THREAD_CPUTIME_ID`, `CLOCK_PROCESS_CPUTIME_ID`, a child's `clock_getcpuclockid` id, and the id of a reaped child. The values must equal the guest's virtual CPU time, matching `getrusage` read beside them, and the reaped child's id must give `EINVAL`, identically across `hermit run --verify` runs.
- T4 (covers F7, F8): raw `syscall(SYS_clock_gettime, ...)` with clock ids 10, 12 and 1000 and with an FD clock id built from a descriptor for `/dev/null`. Linux and the target (owner decision 13) give `EINVAL`; Hermit returns 0 at this SHA.
- T5 (covers F9): a NULL `tp` and `tp` = `(void *)1` both give `EFAULT`.
  Existing candidates: the `clock-gettime-bad-pointer` case in `tests/c/errno_path_identity.c:125` (cell `system-utils/errno-path-identity`, `tests/e2e/manifests/system-utils.yaml:1777`, verify ci on ptrace), and the NULL case in `tests/c/clock_passthrough.c:89`, which runs only with virtual time off.
- T6 (covers F1, F2): with `--no-virtualize-time`, the guest reads the host clock, and a recording replays the recorded values.
  Existing candidates: `run_without_virtual_time_reads_the_host_clock` (`hermit-cli/tests/clock_passthrough.rs:170`) and `record_captures_host_clock_reads_and_replay_returns_them` (`hermit-cli/tests/clock_passthrough.rs:126`), both driving `tests/c/clock_passthrough.c`.
- T7 (covers F10): run Hermit inside a time namespace with a `CLOCK_MONOTONIC` offset, and check that the guest's virtual `CLOCK_MONOTONIC` is the same as without the namespace.
- T8 (covers F2, F3, F4, F5): with a pinned `--epoch`, read `CLOCK_MONOTONIC`, `CLOCK_MONOTONIC_RAW`, `CLOCK_BOOTTIME` and `CLOCK_BOOTTIME_ALARM` next to `/proc/uptime` and `sysinfo().uptime`. All of them must read the virtual uptime, starting near the configured uptime offset, not near the epoch. `CLOCK_TAI` and `CLOCK_REALTIME_ALARM` must read the same as `CLOCK_REALTIME`. The results must be identical across runs and the same on a host without an RTC.
- T9 (covers F8, F9): raw `syscall(SYS_clock_gettime, 10, NULL)` and the same call with a reaped child's CPU clock id must give `EINVAL`, not `EFAULT`, because Linux checks the id before the pointer.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · created for v1: no tracking issue existed for this pinned Sysno; code at this SHA: determinized, dispatch typed.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy model (virtual time answers every clock from epoch plus logical time; --no-virtualize-time records and replays the host clock); linux_determinism nondeterministic (host clocks differ on every read); 10 flag behaviours and 7 tests listed; the handler ignores clockid, so per-clock origins, CPU-time clocks and EINVAL for unknown ids are an open question; no coverage token yet, so tracking_status stays todo
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d615 (owner decisions 6-15): applied 13 and 6 (per-clock virtual realtime, virtual uptime and virtual CPU time; EINVAL for unknown and FD ids, checked before tp); target_policy stays model; new T8, T9; removed the open question on distinguishing clocks

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `clock_gettime`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+clock_gettime&body=Discussion+for+the+%60clock_gettime%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fclock_gettime.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60clock_gettime%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Created by the syscall-tracking v1 migration on 2026-10-03. No tracking issue existed for `clock_gettime`: the 2026-07-24 classification campaign filed issues only for syscalls in the old temporary pass-through block, and Detcore already handled this one.
