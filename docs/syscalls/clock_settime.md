---
schema: syscall-pages/v1
syscall: clock_settime
nr_x86_64: 227
linux_determinism: nondeterministic
target_policy: refuse
tracking_status: todo
cross_host_variant: false
related:
  - https://github.com/rrnewton/hermit/issues/340
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+clock_settime&body=Discussion+for+the+%60clock_settime%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fclock_settime.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60clock_settime%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 239ca43d3092fe64884782ff3166baf34059e41c
  detcore_class: determinized
  dispatch: "typed"
  refused: true
  refused_only_when_fail_closed: false
  subscribed_run: true
  subscribed_record_replay: true
  tests_naming: 0
---

## Summary

`clock_settime(clockid, tp)` sets the value of a system clock. Only `CLOCK_REALTIME`, and dynamic clocks such as PTP devices, can be set, and setting `CLOCK_REALTIME` needs `CAP_SYS_TIME`. Detcore refuses every call with `EPERM` (`detcore/src/lib.rs:2477`), whatever the arguments, so a guest can never change host time or Hermit's virtual clock.

Manual reference: Linux man-pages clock_settime(2).

## Linux semantics

The timekeeping subsystem validates the clock and applies a privileged host- or time-namespace-wide update. The checks run in a fixed order:

1. The clock id must name a clock that has a set operation. An unknown id, or a clock that cannot be set (`CLOCK_MONOTONIC` and the other read-only clocks), gives `EINVAL`.
2. The `timespec` is copied in; a bad pointer gives `EFAULT`. The host strace under Additional information, `clock_settime(-1, -1) = EFAULT`, shows this step: a negative id other than an FD clock is treated as a CPU-time clock, which has a set operation, so the copy is reached.
3. The clock's set operation runs. For `CLOCK_REALTIME` it rejects an invalid `timespec` with `EINVAL`, then checks `CAP_SYS_TIME` in the initial user namespace and gives `EPERM` without it. Root inside a user namespace, such as a container, therefore still gets `EPERM`. CPU-time clocks always give `EPERM` once the clock is validated. A dynamic FD clock is set by its device driver.

A successful `CLOCK_REALTIME` update changes the wall clock for every process on the host. Time namespaces offset only `CLOCK_MONOTONIC` and `CLOCK_BOOTTIME`, and those offsets are written through `/proc`, not through this call.

### Flags and options

- F1: `CLOCK_REALTIME` with a valid `timespec` and `CAP_SYS_TIME` in the initial user namespace: sets the host wall clock for every process and returns 0.
- F2: `CLOCK_REALTIME` without that capability, including root inside a user namespace: `EPERM`.
- F3: `CLOCK_REALTIME` with an invalid `timespec`, for example `tv_nsec` outside 0 to 999,999,999 or a negative `tv_sec`: `EINVAL`, checked before the capability.
- F4: a valid clock that cannot be set: `CLOCK_MONOTONIC`, `CLOCK_MONOTONIC_RAW`, `CLOCK_MONOTONIC_COARSE`, `CLOCK_REALTIME_COARSE`, `CLOCK_BOOTTIME`, `CLOCK_TAI`, `CLOCK_REALTIME_ALARM` and `CLOCK_BOOTTIME_ALARM` give `EINVAL`, before the pointer is read.
- F5: CPU-time clocks: `CLOCK_PROCESS_CPUTIME_ID`, `CLOCK_THREAD_CPUTIME_ID`, and per-process or per-thread ids from `clock_getcpuclockid` or `pthread_getcpuclockid`. `EPERM` for a valid clock, `EINVAL` for one naming a process or thread that does not exist.
- F6: dynamic FD clocks, whose id is built from a file descriptor for a clock device such as `/dev/ptp0`: the device's clock is set, `EACCES` if the descriptor was not opened for writing, and an error if the descriptor is not a clock device.
- F7: an unknown clock id, such as 10 or any id above `CLOCK_TAI`: `EINVAL`.
- F8: a bad `tp` pointer for a clock that can be set: `EFAULT`.

## Determinism analysis

`linux_determinism: nondeterministic`, kept from the legacy classification. Passing through would mutate host time and make guest-visible clocks depend on privilege and concurrent host timekeeping. A successful set changes what every later clock read returns, for the guest and for every process on the host, and which errno a failing call returns depends on the caller's privilege. Only the pure validation failures (F3, F4, F7, F8) repeat on every host.

### Nondeterminism sources

- Host wall-clock state
- Privilege and time namespaces
- Concurrent clock readers
- Concurrent host time adjustment, for example NTP, racing a set

## Hermit design

Target policy: `refuse`. The dispatch arm returns `EPERM` unconditionally (`detcore/src/lib.rs:2477`), before any argument is read, and it is not conditional on `--no-virtualize-time`, so the refusal is the same under `hermit run`, `--no-virtualize-time` and record or replay. The host clock is never touched. The guest's virtual clock, built from the configured epoch plus logical time (`guest_clock_time`, `detcore/src/syscalls/time.rs:95`), is not changed either. `EPERM` is what an unprivileged process inside a user namespace gets on Linux (F2), so a program that handles that case runs unchanged.

The refusal is a fixed answer that needs no host state. `clock_settime` is classified as determinized (`detcore/src/syscall_classification.rs:226`) and listed in the deterministic-refusal set (`detcore/src/syscall_classification.rs:1339`). Backends that run guest syscalls outside Detcore's dispatcher consult that set, through `is_deterministically_refused_syscall` (`detcore/src/lib.rs:226`), to enforce the same refusal when the fail-closed policy is active. The unit test `deterministic_refusal_aggregate_includes_fixed_error_families` (`detcore/src/syscall_classification.rs:2710`) pins membership in that set.

Why `refuse` and not the alternatives:

- `passthrough` would let a guest change host time (F1), and its result would depend on host privilege.
- `model`, a guest-local settable `CLOCK_REALTIME` laid over virtual time, is possible: the offset would be deterministic. But this body knows of no workload that needs it, and it would change what every later clock read returns. It is listed under Open questions.

Where Hermit's answer differs from Linux's:

- F3, F4, F6 and F7 give `EINVAL` (or `EACCES` for F6) on Linux, and F8 gives `EFAULT`. Hermit returns `EPERM` for all of them, because the arm does not look at the arguments. Each Linux answer is computable from the arguments alone, so following Linux's order of checks would stay deterministic.
- F1: privileged Linux succeeds and Hermit refuses. That is deliberate.
- The arm carries `TODO-HUMAN-REVIEW(#663)`, which names https://github.com/rrnewton/hermit/pull/663; the refusal awaits human review.

Target (owner decision 6): the refusal keeps its fixed `EPERM`, returned before any argument is read, which is what the arm does today (`detcore/src/lib.rs:2477`). Linux never lets an unprivileged process set `CLOCK_REALTIME` or a CPU-time clock, so the exception for calls Linux allows unprivileged does not apply to them. The one unprivileged success on Linux is a dynamic FD clock whose device, such as `/dev/ptp0`, the caller could open for writing (F6). This body keeps that case refused too, because setting it would change a host device's clock; that is a judgement call for review. The `EINVAL`, `EFAULT` and `EACCES` answers above are therefore deliberate differences from Linux, not gaps. A program that tries to set the clock, such as `date -s`, `hwclock` or an NTP client, gets the `EPERM` an unprivileged user gets on Linux and reports a permission failure or keeps the clock it has. Owner decision 13 moves every clock read onto virtual time, but it leaves this refusal alone: `clock_settime` changes no virtual clock.

## Recommended tests

- T1 (covers F1, F2, F5): under `hermit run --verify` on ptrace, call raw `syscall(SYS_clock_settime, ...)` for `CLOCK_REALTIME` (current time plus 1 s) and for `CLOCK_PROCESS_CPUTIME_ID`. Assert `-1`/`EPERM` each time, and that a following `clock_gettime(CLOCK_REALTIME)` did not jump by a second.
  Related existing test: `tests/rust/clock_gettime.rs`, run by `default_cargo_clock_gettime` (`hermit-cli/tests/hermit_modes.rs:897`), calls `clock_settime` on ten clock ids but tolerates failure, so it does not pin the errno.
- T2 (covers F3, F4, F7): an invalid `timespec` (`tv_nsec` of 1,000,000,000) on `CLOCK_REALTIME`, the read-only clocks `CLOCK_MONOTONIC`, `CLOCK_BOOTTIME` and `CLOCK_TAI`, and clock ids 10 and 12. Assert `EPERM` for each, as the target requires (owner decision 6), where Linux gives `EINVAL`.
- T3 (covers F6, F8): `tp` = `(void *)1` on `CLOCK_REALTIME` and on `CLOCK_PROCESS_CPUTIME_ID`, and a dynamic FD clock id built from a descriptor for `/dev/null`. Assert `EPERM` for each, as the target requires (owner decision 6), where Linux gives `EFAULT` and `EINVAL`. The errno-identity fixture `tests/c/errno_path_identity.c` (cell `system-utils/errno-path-identity`, `tests/e2e/manifests/system-utils.yaml:1777`, verify ci on ptrace) is a natural home: it already has a `clock-gettime-bad-pointer` case (`tests/c/errno_path_identity.c:125`).
- T4 (covers F2): record and replay a guest whose `clock_settime(CLOCK_REALTIME, ...)` is refused; replay must return the same `EPERM`, and the same must hold under `hermit run --no-virtualize-time`.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-09-28 · hermit 81ca8822f78c · hermit2 · closed under the legacy rule (an explicit `classify_syscall` arm), not the v1 done rule; closing comment https://github.com/rrnewton/hermit/issues/340#issuecomment-5872952587
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant A2 of https://github.com/rrnewton/hermit/issues/340: linux_determinism nondeterministic (from **Classification:**); target_policy from legacy NEEDS-HANDLER; an agent chooses model, emulate or refuse; code at this SHA: determinized, dispatch typed, refused.
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · reopen: under the owner rule of 2026-10-02, closed means every flag and option is covered by a named test and the implementation works; clock_settime records no test coverage, so tracking_status is todo and https://github.com/rrnewton/hermit/issues/340 reopens.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy refuse (the dispatch arm returns a fixed EPERM for every call, in every mode, and the syscall is in the deterministic-refusal set); linux_determinism stays nondeterministic; 8 flag behaviours and 4 tests listed; open questions on following Linux's EINVAL/EFAULT order and on a guest-local settable clock, so tracking_status stays todo
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d615 (owner decisions 6-15): applied 6 (fixed EPERM before any argument check, as the arm does today; the unprivileged exception does not apply) and noted 13 leaves the refusal alone; target_policy stays refuse; no new F/T ids (T2 and T3 now assert EPERM); removed the open question on following Linux's EINVAL/EFAULT order

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `clock_settime`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+clock_settime&body=Discussion+for+the+%60clock_settime%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fclock_settime.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60clock_settime%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: clock_settime" (https://github.com/rrnewton/hermit/issues/340), legacy variant A2.

Comment history: 1 comment(s) on https://github.com/rrnewton/hermit/issues/340.

### Open questions

- Should Hermit ever let a guest set a guest-local `CLOCK_REALTIME`, as an offset over virtual time, instead of refusing? No known workload needs it. Owner decision.

### 2026-07-24 host strace evidence

Test host: Linux 6.17.13 x86_64; strace 6.12.

Ran a syscall-specific safe raw probe with deliberately inert/invalid arguments so privileged or mutating operations could not change host state:

```sh
strace -f -qq -e trace=clock_settime -s 160 python3 -c 'import ctypes; c=ctypes.CDLL(None,use_errno=True); c.syscall(227, *([ctypes.c_long(-1)]*6))'
```

Actual strace output:

```text
clock_settime(0xffffffff /* CLOCK_??? */, 0xffffffffffffffff) = -1 EFAULT (Bad address)
```

The trace confirms the host kernel path and observed errno/result for this ABI. The determinism recommendation also accounts for documented success-path state dependencies; an invalid-argument result alone is not treated as proof of determinism.

### Legacy closure

Closed 2026-09-28 by the 2026-09-28 sweep, which closed issues whose syscall had an explicit `classify_syscall` arm. Closing comment: https://github.com/rrnewton/hermit/issues/340#issuecomment-5872952587
