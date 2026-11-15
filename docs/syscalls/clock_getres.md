---
schema: syscall-pages/v1
syscall: clock_getres
nr_x86_64: 229
linux_determinism: conditional
target_policy: model
tracking_status: todo
cross_host_variant: false
related: []
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+clock_getres&body=Discussion+for+the+%60clock_getres%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fclock_getres.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60clock_getres%60%2C+or+in+what+the+page+says%3F%0A
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

`clock_getres(clockid, res)` reports the resolution of a clock. With virtual time on, the default, Detcore answers every call with a constant 10 µs resolution, whichever clock is named, and accepts a NULL `res` (`detcore/src/syscalls/time.rs:223`). With `--no-virtualize-time` the call goes to the host through the record/replay path (`detcore/src/lib.rs:2468`).

Manual reference: Linux man-pages clock_getres(2).

## Linux semantics

The kernel checks the clock id first and gives `EINVAL` for an unknown id. If `res` is NULL it returns 0 without storing anything, which programs use to test whether a clock exists. Otherwise it copies the clock's resolution out, giving `EFAULT` for a bad pointer. The resolution depends on the clock type and on the kernel's configuration: high-resolution timer clocks report 1 ns when high-resolution timers are active, and one jiffy otherwise. The coarse clocks always report one jiffy, which depends on the kernel's `HZ` setting, for example 4 ms at `HZ=250`.

### Flags and options

- F1: high-resolution timer clocks: `CLOCK_REALTIME`, `CLOCK_MONOTONIC`, `CLOCK_MONOTONIC_RAW`, `CLOCK_BOOTTIME`, `CLOCK_TAI`, and the alarm clocks `CLOCK_REALTIME_ALARM` and `CLOCK_BOOTTIME_ALARM`. They report 1 ns with high-resolution timers active, one jiffy otherwise. The alarm clocks give `EINVAL` on a host without an RTC wake-up device.
- F2: coarse clocks: `CLOCK_REALTIME_COARSE` and `CLOCK_MONOTONIC_COARSE`. They report one jiffy, which depends on the kernel's `HZ`.
- F3: CPU-time clocks: `CLOCK_PROCESS_CPUTIME_ID`, `CLOCK_THREAD_CPUTIME_ID`, and per-process or per-thread ids from `clock_getcpuclockid` or `pthread_getcpuclockid`. They report 1 ns, and `EINVAL` for an id naming a process or thread that does not exist.
- F4: dynamic FD clocks, whose id is built from a descriptor for a clock device such as `/dev/ptp0`: the resolution the device reports, and an error for a descriptor that is not a clock device.
- F5: an unknown clock id, such as 10 or any id above `CLOCK_TAI`: `EINVAL`.
- F6: a NULL `res`: the clock id is still checked, and the call returns 0 without storing a resolution.
- F7: a bad non-NULL `res`: `EFAULT`.

## Determinism analysis

`linux_determinism: conditional`. The answer has no timing dependence: on one host, with one kernel, every call for a given clock returns the same value. It changes across hosts, because it depends on the kernel's `HZ` setting, on whether high-resolution timers are active, and, for the alarm clocks, on whether the host has an RTC wake-up device. The call therefore repeats on one machine but not across machines, which is what a `--verify` run comparing two hosts would see.

### Nondeterminism sources

- The kernel's `HZ` setting, which fixes the coarse clocks' resolution and the fallback resolution
- Whether high-resolution timers are active on the host
- Whether the host has an RTC wake-up device, for the alarm clocks
- The device driver, for dynamic FD clocks

## Hermit design

Target policy: `model`. With virtual time on, the guest's clocks are Detcore's virtual clock, so the resolution reported should describe that clock and must not depend on the host:

- `handle_clock_getres` (`detcore/src/syscalls/time.rs:223`) writes a constant `timespec` of 0 s and 10,000 ns (`detcore/src/syscalls/time.rs:240`) for every clock id. The comment at `detcore/src/syscalls/time.rs:235` calls this "10ms", but the value written is 10 µs; the doc comment on line 222 also names the wrong syscall (`clock_gettime`). The value matches the base cost charged for one clock read (`CLOCK_NS`, `detcore/src/syscall_time.rs:11`).
- A NULL `res` returns 0 without a write (`detcore/src/syscalls/time.rs:234`). The comment there explains that GHC's runtime probes the per-thread CPU clock with `clock_getres(clockid, NULL)`, so `EFAULT` would abort such programs. That is F6 for valid clock ids.
- A bad non-NULL pointer fails in the memory write (`detcore/src/syscalls/time.rs:243`), which is F7.

With `--no-virtualize-time` the arm calls `passthrough` (`detcore/src/lib.rs:2472`), which records the host's answer and returns it on replay, as the comment at `detcore/src/lib.rs:2444` requires for every clock syscall.

`passthrough` with virtual time on would report the host's resolution for a clock the guest does not read, and would differ between hosts. `refuse` would break programs that probe clocks.

What remains before the code fully realizes `model`: the handler ignores `clockid`.

- F1, F2, F3: every clock reports 10 µs. Linux reports 1 ns for the high-resolution timer and CPU-time clocks on typical modern hosts, and a jiffy for the coarse clocks. A deterministic per-clock table would keep repeatability and be closer to Linux. It needs a decision on what virtual resolution each clock has.
- F1: the alarm clocks never give `EINVAL`.
- F3, F4, F5: ids for a process or thread that does not exist, dynamic FD clock ids, and unknown ids all get 10 µs, or 0 with a NULL `res`, instead of `EINVAL`. A program that probes a clock with `clock_getres(id, NULL)` is told every clock exists.

Target (owner decision 13, with decision 6 for the order of checks): `clock_getres` accepts exactly the ids `clock_gettime` accepts and rejects the rest with `EINVAL`. The clock family is the one the `clock_gettime` body names:

- virtual realtime: `CLOCK_REALTIME`, `CLOCK_REALTIME_COARSE`, `CLOCK_REALTIME_ALARM` and `CLOCK_TAI`;
- virtual uptime: `CLOCK_MONOTONIC`, `CLOCK_MONOTONIC_COARSE`, `CLOCK_MONOTONIC_RAW`, `CLOCK_BOOTTIME` and `CLOCK_BOOTTIME_ALARM`;
- virtual CPU time: `CLOCK_PROCESS_CPUTIME_ID`, `CLOCK_THREAD_CPUTIME_ID`, and the encoded per-process and per-thread ids, with `EINVAL` for an id naming no live guest task;
- `EINVAL` for unknown ids (10, and 12 and up) and for dynamic FD clock ids.

The alarm clocks are accepted on every host, with or without an RTC. Each resolution comes from a fixed per-clock table, never from the host. The numbers in that table are still open (see Open questions).

Kernel order (`SYSCALL_DEFINE2(clock_getres)` in `kernel/time/posix-timers.c`):

1. `clockid_to_kclock` gives `EINVAL`.
2. The clock's `clock_getres` runs, which gives `EINVAL` for a CPU clock naming no task.
3. A NULL `res` returns 0 here.
4. Otherwise `put_timespec64` gives `EFAULT`.

So `clock_getres(id, NULL)` is a working existence probe. Today the handler never reads `clockid` (`detcore/src/syscalls/time.rs:223`) and returns 0 for a NULL `res` whatever the id (`detcore/src/syscalls/time.rs:234`).

## Recommended tests

- T1 (covers F1, F6): under `hermit run --verify` on ptrace, `clock_getres(CLOCK_MONOTONIC)` and `clock_getres(CLOCK_REALTIME)` return the same value in both runs, and a NULL `res` returns 0.
  Existing candidates in the cargo target `tests_time` (`detcore/Cargo.toml:51`): `tod_clock_getres` (`detcore/tests/time/mod.rs:387`), which asserts the constant 10,000 ns for `CLOCK_MONOTONIC`, and `clock_getres_null_res_is_ok` (`detcore/tests/time/mod.rs:414`), which passes NULL for `CLOCK_MONOTONIC` and `CLOCK_THREAD_CPUTIME_ID`.
- T2 (covers F2, F3): read the resolution of the coarse clocks, `CLOCK_PROCESS_CPUTIME_ID`, `CLOCK_THREAD_CPUTIME_ID` and a child's `clock_getcpuclockid` id. Record Hermit's values next to the host's.
  Related existing test: `tests/rust/clock_gettime.rs`, run by `default_cargo_clock_gettime` (`hermit-cli/tests/hermit_modes.rs:897`), reads the resolution of ten clock ids (`tests/rust/clock_gettime.rs:42`) without checking the values.
- T3 (covers F3, F4, F5): raw `syscall(SYS_clock_getres, ...)` with clock ids 10, 12 and 1000, with an FD clock id built from a descriptor for `/dev/null`, and with the id of a reaped child, each with a NULL and a non-NULL `res`. Linux and the target (owner decision 13) give `EINVAL`, with either `res`; Hermit returns 0 at this SHA.
- T4 (covers F7): `res` = `(void *)1` gives `EFAULT`.
- T5 (covers F1, F6): with `--no-virtualize-time`, the host's resolution is reported, recorded, and replayed.
  Existing candidates: `run_without_virtual_time_reads_the_host_clock` (`hermit-cli/tests/clock_passthrough.rs:170`) and `record_captures_host_clock_reads_and_replay_returns_them` (`hermit-cli/tests/clock_passthrough.rs:126`). Their guest, `tests/c/clock_passthrough.c`, calls `clock_getres(CLOCK_MONOTONIC)` (`tests/c/clock_passthrough.c:79`) and `clock_getres(CLOCK_REALTIME, NULL)` (`tests/c/clock_passthrough.c:83`).

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · created for v1: no tracking issue existed for this pinned Sysno; code at this SHA: determinized, dispatch typed.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy model (virtual time reports a constant 10 us resolution for every clock and accepts a NULL res; --no-virtualize-time records and replays the host answer); linux_determinism stays conditional (stable on one host, varies with HZ, high-resolution timers and RTC presence); 7 flag behaviours and 5 tests listed; per-clock resolutions and EINVAL for unknown ids are an open question, so tracking_status stays todo
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d615 (owner decisions 6-15): applied 13 and 6 (same accepted ids and clock family as clock_gettime, EINVAL for unknown, FD and dead-task ids before the NULL-res return, fixed per-clock resolution table); target_policy stays model; no new F/T ids; the per-clock question was narrowed to the table's numbers

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `clock_getres`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+clock_getres&body=Discussion+for+the+%60clock_getres%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fclock_getres.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60clock_getres%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Created by the syscall-tracking v1 migration on 2026-10-03. No tracking issue existed for `clock_getres`: the 2026-07-24 classification campaign filed issues only for syscalls in the old temporary pass-through block, and Detcore already handled this one.

### Open questions

- Which number should each clock report in the fixed per-clock table? For example: 1 ns for the high-resolution timer and CPU-time clocks, and a fixed jiffy for the coarse clocks. Owner decision 13 settles which ids exist and that they get `EINVAL` otherwise, but not these values. The current constant 10 µs matches the virtual cost of one clock read. Changing it alters what guests see, and `tod_clock_getres` pins the constant.
- The comment at `detcore/src/syscalls/time.rs:235` says "10ms" for a value of 10 µs. That is a code comment fix, outside this body's scope.
