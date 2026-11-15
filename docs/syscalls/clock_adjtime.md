---
schema: syscall-pages/v1
syscall: clock_adjtime
nr_x86_64: 305
linux_determinism: nondeterministic
target_policy: model
tracking_status: todo
cross_host_variant: false
related:
  - https://github.com/rrnewton/hermit/issues/339
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+clock_adjtime&body=Discussion+for+the+%60clock_adjtime%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fclock_adjtime.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60clock_adjtime%60%2C+or+in+what+the+page+says%3F%0A
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

Reads or adjusts clock-discipline parameters for a specified clock: the clock-id form of `adjtimex`. With virtual time, Hermit answers a query on `CLOCK_REALTIME` with a fixed, unsynchronized discipline and the virtual realtime clock, refuses every change with `EPERM`, and returns `EOPNOTSUPP` for every other clock id.

Manual reference: Linux man-pages clock_adjtime(2).

## Linux semantics

`clock_adjtime(clk_id, buf)` copies `struct timex` in from `buf`, applies the requested `modes` to the clock `clk_id`, and copies the resulting discipline back out. The return value is the clock state (`TIME_OK` through `TIME_ERROR`). For `CLOCK_REALTIME` the kernel uses the same discipline code as `adjtimex`; other clocks either have their own adjustment routine or do not support adjustment. The structure is copied in before the clock id is looked up, so a bad `buf` gives `EFAULT` whatever the clock id.

### Flags and options

- F1: `clk_id = CLOCK_REALTIME`: the system-wide realtime clock, handled exactly as `adjtimex`.
- F2: other fixed clock ids (`CLOCK_MONOTONIC`, `CLOCK_MONOTONIC_RAW`, `CLOCK_BOOTTIME`, `CLOCK_TAI`, the coarse and alarm clocks): no adjustment routine, so `EOPNOTSUPP`.
- F3: an invalid clock id: `EINVAL`.
- F4: dynamic clocks (a negative id encoding an open file descriptor, such as a PTP hardware clock `/dev/ptpN`): adjusted by the device driver. An adjustment needs the descriptor opened for writing (`EACCES` otherwise); a query only needs it open.
- F5: CPU-time clocks (`CLOCK_PROCESS_CPUTIME_ID`, `CLOCK_THREAD_CPUTIME_ID`, and the negative per-pid and per-thread CPU clock ids): `EOPNOTSUPP`.
- F6: auxiliary clocks (`CLOCK_AUX` ids, Linux 6.17): independently adjustable clocks with their own discipline.
- F7: `modes == 0`: read-only query of the clock's discipline.
- F8: `modes == ADJ_OFFSET_SS_READ`: read-only query of the remaining `adjtime` slew in `offset`.
- F9: mutating modes, alone or combined: `ADJ_OFFSET`, `ADJ_FREQUENCY`, `ADJ_MAXERROR`, `ADJ_ESTERROR`, `ADJ_STATUS`, `ADJ_TIMECONST`, `ADJ_TAI`, `ADJ_SETOFFSET`, `ADJ_MICRO`, `ADJ_NANO`, `ADJ_TICK` and `ADJ_OFFSET_SINGLESHOT`. On `CLOCK_REALTIME` these need `CAP_SYS_TIME` in the initial user namespace (`EPERM` otherwise), and invalid values (for example an out-of-range `tick`, or an `ADJ_SETOFFSET` with a bad `time.tv_usec`) give `EINVAL`.
- F10: `ADJ_ADJTIME` (`0x8000`) without `ADJ_OFFSET_SINGLESHOT`: `EINVAL`, checked before the capability. Other unknown `modes` bits are not rejected as such: without `ADJ_ADJTIME`, any non-zero `modes` from a caller without `CAP_SYS_TIME` gives `EPERM` (`timekeeping_validate_timex`).
- F11: return value: the clock state, `TIME_ERROR` when the clock is unsynchronized.
- F12: `buf` NULL or unmapped: `EFAULT`, checked before the clock id.
- F13: output fields written to `buf`: `offset`, `freq`, `maxerror`, `esterror`, `status`, `constant`, `precision`, `tolerance`, `time`, `tick` and the PPS and `tai` fields.
- F14: `ADJ_OFFSET_SS_READ` combined with other `modes` bits: the `ADJ_ADJTIME` path ignores the other bits, so the call is a read-only query that needs no capability, except that `ADJ_SETOFFSET` still needs `CAP_SYS_TIME` (`EPERM`), because `timekeeping_validate_timex` checks it whatever `ADJ_ADJTIME` says.

## Determinism analysis

A query reports live host state: whether NTP or PTP has synchronized the clock, the current frequency correction and error estimates, any pending slew, and the wall-clock `time`. These change from host to host and from moment to moment, so the call is `nondeterministic`. The error paths that do not depend on the host (F2, F3, F5, F10, F12, and F9 for an unprivileged caller) repeat.

### Nondeterminism sources

- Live clock discipline and wall time (F11, F13).
- Clock ID and host support: dynamic clocks exist only where the device does (F4); auxiliary clocks need Linux 6.17 (F6).
- Privileged global configuration: a privileged caller changes the host clock for every process (F9).

## Hermit design

Target policy: `model`, the same policy as `adjtimex`, with which it shares its code. Code at hermit f4055eb9cae4:

- Dispatch (`detcore/src/lib.rs:2501`): with `virtualize_time` the call goes to `handle_clock_adjtime` (`detcore/src/syscalls/time.rs:265`), tagged `TODO-HUMAN-REVIEW(PR-857)` (`detcore/src/lib.rs:2500`).
- Any clock id other than `CLOCK_REALTIME` returns `EOPNOTSUPP` (`detcore/src/syscalls/time.rs:270`-`271`). That matches Linux for F2 and F5.
- For `CLOCK_REALTIME`, `write_deterministic_timex` (`detcore/src/syscalls/time.rs:276`) does what it does for `adjtimex`: a NULL `buf` gives `EFAULT` (`detcore/src/syscalls/time.rs:281`); the structure is read from guest memory (`detcore/src/syscalls/time.rs:282`); any `modes` value other than `0` or `ADJ_OFFSET_SS_READ` (`timex_mode_is_query`, `detcore/src/syscalls/time.rs:74`) returns `EPERM` (`detcore/src/syscalls/time.rs:284`), so the host clock is never changed (F9); a query writes `deterministic_timex` (`detcore/src/syscalls/time.rs:80`), a zeroed structure with `status = STA_UNSYNC`, `tick = 10000` and `time` from `thread_observe_time` (`detcore/src/syscalls/time.rs:287`), and returns `TIME_ERROR` (`detcore/src/syscalls/time.rs:289`), Linux's answer for an unsynchronized clock (F11).
- `clock_adjtime` is listed as a call whose main result is a clock value (`detcore/src/syscall_time.rs:225`), so each query is charged virtual time.
- Classification: Determinized (`detcore/src/syscall_classification.rs:620`), checked by `clock_discipline_and_kernel_log_syscalls_are_determinized` (`detcore/src/syscall_classification.rs:2175`).

Differences from Linux that remain:

- F3: an invalid clock id gets `EOPNOTSUPP` instead of `EINVAL`.
- F12: the clock id is checked before `buf` is read, so a NULL or unmapped `buf` with a clock id other than `CLOCK_REALTIME` gets `EOPNOTSUPP` instead of `EFAULT`.
- F4: dynamic clocks get `EOPNOTSUPP`, including a query on a PTP descriptor the guest legitimately opened, and including the `EACCES` case.
- F6: auxiliary clocks get `EOPNOTSUPP`, which a 6.17 kernel would not return.
- F10: undefined `modes` bits get `EPERM` instead of `EINVAL`.
- F13: the snapshot zeroes `maxerror`, `esterror`, `tolerance`, `precision` and `constant`, which no real unsynchronized kernel reports, and `time` comes from `thread_observe_time` while `clock_gettime` goes through `guest_clock_time` (`detcore/src/syscalls/time.rs:95`). The `adjtimex` body records the same two points.

Target (owner decisions 6 and 13): `clock_adjtime` is modelled, so it owes Linux's checks in kernel order, and an unknown clock id gets `EINVAL`. The order is that of `SYSCALL_DEFINE2(clock_adjtime)` and `do_clock_adjtime()` (`kernel/time/posix-timers.c`), then `timekeeping_validate_timex()` (`kernel/time/timekeeping.c`) for `CLOCK_REALTIME`, judged for a caller without `CAP_SYS_TIME`, which is what the guest is: copy `buf` in (`EFAULT`, F12); look the clock id up (`EINVAL` for an id that is no clock, F3); a valid clock without an adjustment routine gives `EOPNOTSUPP` (F2, F5); then `ADJ_ADJTIME` without `ADJ_OFFSET_SINGLESHOT` gives `EINVAL` (F10), `ADJ_ADJTIME` without `ADJ_OFFSET_READONLY` gives `EPERM`, any other non-zero `modes` gives `EPERM` (F9), and `ADJ_SETOFFSET` gives `EPERM` (F14); what passes is a query (F7, F8, F14). Today the clock-id test runs first and answers `EOPNOTSUPP` for every id other than `CLOCK_REALTIME` (`detcore/src/syscalls/time.rs:270`-`271`), before `buf` is read (`detcore/src/syscalls/time.rs:281`-`282`), and `timex_mode_is_query` (`detcore/src/syscalls/time.rs:74`) accepts only `0` and `ADJ_OFFSET_SS_READ` exactly, so F3, F10, F12 and F14 differ. The change: read `buf` first, then classify the clock id (the fixed ids other than `CLOCK_REALTIME`, and the CPU-time ids, give `EOPNOTSUPP` as today; an id that names no clock gives `EINVAL`), then apply the mode rules above. The query keeps reporting the virtual realtime clock (decision 13). Dynamic (descriptor-encoded) and auxiliary ids are not settled by these decisions; they stay with the open question below.

Without virtual time the arm calls `handle_unsupported_syscall` (`detcore/src/lib.rs:2505`). The record/replay configuration sets `panic_on_unsupported_syscalls` and `exit_on_unsupported_syscall` (`hermit-cli/src/metadata.rs:391`, `hermit-cli/src/metadata.rs:394`), so a recording that reaches `clock_adjtime` stops with an `UnsupportedSyscallError` tool error (`detcore/src/lib.rs:398`). In a `hermit run` without virtual time and without `panic_on_unsupported_syscalls`, the call is reported and forwarded to the host (`detcore/src/lib.rs:404`, `detcore/src/lib.rs:405`).

Why `model` and not the alternatives: `passthrough` leaks host NTP state and the wall clock and lets a privileged guest change the host clock; `refuse` would break `ntp_gettime`-style queries, which glibc can issue through this call (`detcore/src/syscall_time.rs:188`); `emulate` as a no-op could not supply a `time` field consistent with the virtual clock.

## Recommended tests

- T1 (covers F1, F7, F11, F13): a `modes == 0` query on `CLOCK_REALTIME` under `hermit run --verify` on ptrace returns 5 (`TIME_ERROR`), `status` 64 (`STA_UNSYNC`) and `tick` 10000, identically across runs; also check that `time` lies between two surrounding `clock_gettime(CLOCK_REALTIME)` reads and that the zeroed fields are as the design states.
  Existing candidates: `tests/c/clock_adjtime_deterministic.c` (query at `tests/c/clock_adjtime_deterministic.c:20`, cell `c-programs/clock-adjtime-deterministic`, `tests/e2e/manifests/c-programs.yaml:516`, verify `ci` `ptrace: true` at `tests/e2e/manifests/c-programs.yaml:535`), and `clock_discipline_and_kernel_log_are_host_independent` (`hermit-cli/tests/clock_discipline_determinism.rs:94`), which runs the same guest with `--backend=ptrace` under `run --strict` and `run --strict --verify` and expects `clock-adjtime-ok state=5 status=64 tick=10000` (`hermit-cli/tests/clock_discipline_determinism.rs:25`). Neither checks `time` or the other fields. The test inventory (`tests/e2e/manifests/inventory/test-files.json:313`) describes the cell as `ci=false`, which disagrees with the manifest line.
- T2 (covers F8): `ADJ_OFFSET_SS_READ` on `CLOCK_REALTIME` returns the T1 snapshot with `offset` 0.
- T3 (covers F9): each mutating mode listed in F9 returns `EPERM` on `CLOCK_REALTIME`, and a following query is unchanged. `tests/c/clock_adjtime_deterministic.c:26`-`36` covers only `ADJ_OFFSET`.
- T4 (covers F10): `modes = 0x8000` on `CLOCK_REALTIME` must return `EINVAL`, as on Linux (owner decision 6). Hermit at f4055eb9cae4 returns `EPERM`, so the test fails until the mode checks follow the kernel's order.
- T5 (covers F2, F5): a query on `CLOCK_MONOTONIC`, `CLOCK_BOOTTIME`, `CLOCK_TAI`, `CLOCK_PROCESS_CPUTIME_ID` and a per-pid CPU clock returns `EOPNOTSUPP`, as on Linux.
- T6 (covers F3, F12): an invalid clock id, and a NULL `buf` with both `CLOCK_REALTIME` and `CLOCK_MONOTONIC`. The target (owner decisions 6 and 13) is Linux's answer: `EINVAL` for the invalid id with a valid `buf`, and `EFAULT` for both NULL cases. Hermit at f4055eb9cae4 returns `EOPNOTSUPP`, `EFAULT` and `EOPNOTSUPP`.
- T7 (covers F4, F6): a query on a dynamic clock id built from an open descriptor, and on an auxiliary clock id; both return `EOPNOTSUPP` under Hermit. Skip where the host has no PTP device or runs a kernel before 6.17 for the Linux comparison.
- T8 (covers F1): record and replay a guest that queries `clock_adjtime`. By the code the recording stops with an unsupported-syscall error; confirm that and turn it into a test of whatever record/replay policy is chosen.
- T9 (covers F14): on `CLOCK_REALTIME`, `modes = ADJ_OFFSET_SS_READ | ADJ_STATUS` must return the T1 snapshot as a query, and `modes = ADJ_OFFSET_SS_READ | ADJ_SETOFFSET` must return `EPERM`, as on Linux (owner decision 6). Hermit at f4055eb9cae4 returns `EPERM` for both.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-09-28 · hermit 81ca8822f78c · hermit2 · closed under the legacy rule (an explicit `classify_syscall` arm), not the v1 done rule; closing comment https://github.com/rrnewton/hermit/issues/339#issuecomment-5872949745
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant A2 of https://github.com/rrnewton/hermit/issues/339: linux_determinism nondeterministic (from **Classification:**); target_policy from legacy NEEDS-HANDLER; an agent chooses model, emulate or refuse; code at this SHA: determinized, dispatch typed.
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · reopen: under the owner rule of 2026-10-02, closed means every flag and option is covered by a named test and the implementation works; clock_adjtime records no test coverage, so tracking_status is todo and https://github.com/rrnewton/hermit/issues/339 reopens.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy investigate -> model (time.rs:265 answers CLOCK_REALTIME queries with the shared adjtimex snapshot, EPERM for changes, EOPNOTSUPP for every other clock id), matching adjtimex; linux_determinism stays nondeterministic; 13 flag behaviours and 8 tests listed; open questions on the errno order for invalid ids and NULL buf, dynamic and aux clocks, and record mode; no coverage token yet, so tracking_status stays todo
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d615 (owner decisions 6-15): decisions 6 and 13 applied: target is Linux's check order (EFAULT for buf, EINVAL for an unknown clock id, EOPNOTSUPP for an unadjustable one, then the timekeeping_validate_timex mode rules); F10 narrowed to Linux's actual ADJ_ADJTIME rule; new F14 (ADJ_OFFSET_SS_READ with extra bits) and T9; T4 and T6 now assert Linux's answers; target_policy stays model; open question on the error order removed

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `clock_adjtime`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+clock_adjtime&body=Discussion+for+the+%60clock_adjtime%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fclock_adjtime.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60clock_adjtime%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: clock_adjtime" (https://github.com/rrnewton/hermit/issues/339), legacy variant A2.

Comment history: 1 comment(s) on https://github.com/rrnewton/hermit/issues/339.

Rank 60 in the batch-2 classification sequence after excluding the 50 workload-observed batch-1 syscalls.

### Open questions

- Should a query on a dynamic (PTP) clock or a 6.17 auxiliary clock be modeled instead of refused? No test or workload in the tree needs it.
- Record/replay: should `clock_adjtime` be modeled in record mode too, instead of stopping the recording? This is the same question as for `adjtimex`.

### 2026-07-24 host strace evidence

Test host: Linux 6.17.13 x86_64; strace 6.12.

Ran a syscall-specific safe raw probe with deliberately inert/invalid arguments so privileged or mutating operations could not change host state:

```sh
strace -f -qq -e trace=clock_adjtime -s 160 python3 -c 'import ctypes; c=ctypes.CDLL(None,use_errno=True); c.syscall(305, *([ctypes.c_long(-1)]*6))'
```

Actual strace output:

```text
clock_adjtime(0xffffffff /* CLOCK_??? */, 0xffffffffffffffff) = -1 EFAULT (Bad address)
```

The trace confirms the host kernel path and observed errno/result for this ABI. The determinism recommendation also accounts for documented success-path state dependencies; an invalid-argument result alone is not treated as proof of determinism. It also shows Linux checking `buf` before the clock id: under Hermit the same call returns `EOPNOTSUPP` (`detcore/src/syscalls/time.rs:271`).

### Legacy closure

Closed 2026-09-28 by the 2026-09-28 sweep, which closed issues whose syscall had an explicit `classify_syscall` arm. Closing comment: https://github.com/rrnewton/hermit/issues/339#issuecomment-5872949745
