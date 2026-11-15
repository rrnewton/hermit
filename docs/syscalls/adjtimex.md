---
schema: syscall-pages/v1
syscall: adjtimex
nr_x86_64: 159
linux_determinism: nondeterministic
target_policy: model
tracking_status: todo
cross_host_variant: false
related:
  - https://github.com/rrnewton/hermit/issues/333
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+adjtimex&body=Discussion+for+the+%60adjtimex%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fadjtimex.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60adjtimex%60%2C+or+in+what+the+page+says%3F%0A
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

`adjtimex(buf)` reads the kernel's NTP clock-discipline state into `struct timex` and, with nonzero `modes`, changes it. Under virtual time Detcore models the call (`detcore/src/syscalls/time.rs:253`): the two query forms report a fixed, unsynchronized discipline whose `time` field is the guest's virtual clock, and every mode that would change the host clock gets `EPERM`. Without virtual time (record/replay, `--no-virtualize-time`) the call goes to the unsupported-syscall path (`detcore/src/lib.rs:2486`).

Manual reference: Linux man-pages adjtimex(2).

## Linux semantics

The call copies `struct timex` in, validates `modes`, applies the requested changes to the global timekeeping state, then copies the whole structure back with the current discipline: `offset`, `freq`, `maxerror`, `esterror`, `status`, `constant`, `precision`, `tolerance`, `tick`, `tai`, the PPS counters, and `time`, the current `CLOCK_REALTIME` value. `time.tv_usec` holds microseconds, or nanoseconds when `STA_NANO` is set in `status`. The return value is the clock state: `TIME_OK` (0), `TIME_INS` (1), `TIME_DEL` (2), `TIME_OOP` (3), `TIME_WAIT` (4), or `TIME_ERROR` (5), which is returned whenever `STA_UNSYNC` (0x40) is set.

The state is one per host, shared by every process and container on the machine. Changing it needs `CAP_SYS_TIME` in the initial user namespace, so a root user inside an unprivileged user namespace still gets `EPERM`.

Validation runs in this order (kernel `timekeeping_validate_timex`): first the old-style `adjtime` bit (`0x8000`) is checked, then privilege for any other nonzero `modes`, then the `ADJ_TICK` range, then `ADJ_SETOFFSET`'s privilege and value, then the `ADJ_FREQUENCY` overflow bound. A bad `buf` fails with `EFAULT` before any of these.

### Flags and options

- F1: `modes == 0`: pure query. No privilege needed. Fills the structure with the live discipline and returns the clock state (`TIME_ERROR` on an unsynchronized host).
- F2: `ADJ_OFFSET_SS_READ` (0xa001): read-only `adjtime(3)` query. No privilege needed. `offset` returns the remaining `adjtime` slew in microseconds; nothing changes.
- F3: `ADJ_OFFSET_SINGLESHOT` (0x8001): old-style `adjtime(3)` slew by `offset` microseconds. Needs `CAP_SYS_TIME`; without it `EPERM`. `offset` returns the previous remaining slew.
- F4: the `adjtime` bit `0x8000` without `ADJ_OFFSET` (0x0001), for example `0x8000` or `0xa000`: `EINVAL`, checked before privilege, so an unprivileged caller also gets `EINVAL`. Extra bits next to `ADJ_OFFSET_SS_READ` (for example `0xa003`) are ignored by the `adjtime` path, so that call is still a read-only query. The exception is `ADJ_SETOFFSET`, for which `timekeeping_validate_timex` still demands `CAP_SYS_TIME` (`EPERM`).
- F5: NTP discipline writes `ADJ_OFFSET` (0x0001), `ADJ_FREQUENCY` (0x0002), `ADJ_MAXERROR` (0x0004), `ADJ_ESTERROR` (0x0008), `ADJ_STATUS` (0x0010), `ADJ_TIMECONST` (0x0020) and `ADJ_TAI` (0x0080): each needs `CAP_SYS_TIME`, else `EPERM`. A privileged `ADJ_FREQUENCY` value that would overflow the scaled frequency gives `EINVAL`.
- F6: `ADJ_TICK` (0x4000): needs `CAP_SYS_TIME`, else `EPERM`. For a privileged caller, a `tick` outside 900000/USER_HZ..1100000/USER_HZ gives `EINVAL`.
- F7: `ADJ_SETOFFSET` (0x0100): steps the clock by `time`. Needs `CAP_SYS_TIME`, else `EPERM`; an invalid `time` (microsecond or, with `ADJ_NANO`, nanosecond field out of range) gives `EINVAL`.
- F8: `ADJ_MICRO` (0x1000) and `ADJ_NANO` (0x2000): select the resolution of `offset` and `time` (sets or clears `STA_NANO`). They are modes like the others and need `CAP_SYS_TIME`, else `EPERM`.
- F9: `buf` is NULL or not readable and writable: `EFAULT`.

## Determinism analysis

`linux_determinism: nondeterministic`, kept from the legacy classification. Even the query forms (F1, F2) return host state: the `time` field is the live wall clock, and `status`, `freq`, `offset`, the error estimates and the return code depend on whether and how an NTP daemon on the host is disciplining the clock, which changes from moment to moment and from host to host. The write forms change state shared with every other process on the machine. The call repeats only for the error paths decided by the arguments alone (F4, F9, and `EPERM` for an unprivileged caller).

### Nondeterminism sources

- Host clock and NTP discipline state: synchronization status, frequency correction, pending `adjtime` slew, leap-second state
- Wall-clock progression, through the `time` field
- Privileged global configuration: another process, or the guest itself with `CAP_SYS_TIME`, can change the discipline at any moment

## Hermit design

Target policy: `model`. Detcore answers queries from its own virtual clock with a fixed discipline, and refuses every change to the host clock. Code at hermit f4055eb9cae4:

- Dispatch (`detcore/src/lib.rs:2486`): with `virtualize_time` the call goes to `handle_adjtimex` (`detcore/src/syscalls/time.rs:253`), which calls `write_deterministic_timex` (`detcore/src/syscalls/time.rs:276`). The same helper serves `clock_adjtime(CLOCK_REALTIME, ...)` (`detcore/src/syscalls/time.rs:265`).
- A NULL `buf` gives `EFAULT` (`detcore/src/syscalls/time.rs:281`), and the structure is read from guest memory before anything else (`detcore/src/syscalls/time.rs:282`), so an unreadable `buf` fails there (F9).
- `timex_mode_is_query` (`detcore/src/syscalls/time.rs:74`) accepts exactly `modes == 0` and `modes == ADJ_OFFSET_SS_READ`. Every other value returns `EPERM` (`detcore/src/syscalls/time.rs:283`). This makes the guest behave as an unprivileged caller for F3, F5, F6, F7 and F8, which keeps the host clock untouched, and matches `clock_settime`, which Detcore also answers with `EPERM` (`detcore/src/lib.rs:2477`). The guest is told it is uid 0 (`detcore/src/lib.rs:2760`), so a guest that checks its uid before calling `adjtimex` will still see a refusal, as root in an unprivileged user namespace would on Linux.
- For a query, `deterministic_timex` (`detcore/src/syscalls/time.rs:80`) writes a zeroed structure with `status = STA_UNSYNC`, `tick = 10000`, and `time` set to the virtual realtime clock read through `thread_observe_time` (`detcore/src/syscalls/time.rs:287`). The call returns `TIME_ERROR` (`detcore/src/syscalls/time.rs:289`), as Linux does for an unsynchronized clock. `offset` is 0, which is Linux's answer to F2 when no `adjtime` slew is pending.
- `adjtimex` is listed as a call whose main result is a clock value (`detcore/src/syscall_time.rs:224`), so it is charged virtual time even inside a backend bootstrap window, and two queries return different `time` values.

Differences from Linux that remain:

- F4: `0x8000` without `ADJ_OFFSET` gets `EPERM` under Hermit and `EINVAL` on Linux; `0xa003` and other read-only `adjtime` calls with extra bits are queries on Linux and get `EPERM` under Hermit.
- The modeled snapshot zeroes `maxerror`, `esterror`, `tolerance`, `precision` and `constant`. An unsynchronized Linux host reports nonzero values for these (for example the 16 s `NTP_PHASE_LIMIT` error estimates and a 500 ppm tolerance). The values are deterministic, but a program that interprets them sees a discipline no real kernel reports.
- The `time` field comes from `thread_observe_time` directly, while `time` and `gettimeofday` go through `guest_clock_time` (`detcore/src/syscalls/time.rs:95`), which also passes the value through the per-guest clock observer. Whether the two can disagree is not established here.

Target (owner decision 6): `adjtimex` is modelled, so it owes Linux's checks in kernel order, judged for a caller without `CAP_SYS_TIME`, which is what the guest is. The order is `SYSCALL_DEFINE1(adjtimex)` (copy `buf` in, `EFAULT`, F9) then `timekeeping_validate_timex()` (`kernel/time/timekeeping.c`): `ADJ_ADJTIME` (`0x8000`) without `ADJ_OFFSET_SINGLESHOT` gives `EINVAL` (F4); `ADJ_ADJTIME` without `ADJ_OFFSET_READONLY` gives `EPERM` (F3); without `ADJ_ADJTIME`, any non-zero `modes` gives `EPERM` (F5, F6, F7, F8); `ADJ_SETOFFSET` gives `EPERM` in either case; what passes is a query (F1, F2, and F4's `0xa003`). Today `timex_mode_is_query` (`detcore/src/syscalls/time.rs:74`) accepts only `0` and `ADJ_OFFSET_SS_READ` exactly and everything else gets `EPERM` (`detcore/src/syscalls/time.rs:283`-`284`), so F4 differs in both directions. The change is to replace that predicate with the rules above; F1, F2, F3, F5 to F9 already give Linux's answer. Decision 13 needs nothing more here: the query already reports the virtual realtime clock.

Without virtual time the arm calls `handle_unsupported_syscall` (`detcore/src/lib.rs:2490`). The record/replay configuration sets `panic_on_unsupported_syscalls` and `exit_on_unsupported_syscall` (`hermit-cli/src/metadata.rs:391`, `hermit-cli/src/metadata.rs:394`), so a recording that reaches `adjtimex` gets an `UnsupportedSyscallError` tool error (`detcore/src/lib.rs:398`) instead of a recorded value. In a `hermit run --no-virtualize-time` without `panic_on_unsupported_syscalls`, the call is reported and forwarded to the host (`detcore/src/lib.rs:404`, `detcore/src/lib.rs:405`), so the guest sees the host's discipline.

Why `model` and not the alternatives: `passthrough` leaks host NTP state and the wall clock (and would let a privileged guest change the host clock); `refuse` would break `ntp_gettime`-style queries that programs and libraries issue for the clock state; `emulate` as a no-op could not supply a `time` field consistent with the virtual clock.

## Recommended tests

- T1 (covers F1): a `modes == 0` query under `hermit run --verify` on ptrace returns 5 (`TIME_ERROR`), `status` 64 (`STA_UNSYNC`) and `tick` 10000, identically across runs. Also check that `time` lies between two surrounding `clock_gettime(CLOCK_REALTIME)` reads.
  Existing candidates: `tests/c/adjtimex_deterministic.c` (cell `c-programs/adjtimex-deterministic`, `tests/e2e/manifests/c-programs.yaml:257`, verify `ci` `ptrace: true` at `tests/e2e/manifests/c-programs.yaml:276`), and `clock_discipline_and_kernel_log_are_host_independent` at `hermit-cli/tests/clock_discipline_determinism.rs:94`, which runs the same guest with `--backend=ptrace` under `run --strict` and `run --strict --verify` and expects `adjtimex-ok state=5 status=64 tick=10000` (`hermit-cli/tests/clock_discipline_determinism.rs:20`). Neither checks the `time` field. The test inventory (`tests/e2e/manifests/inventory/test-files.json:247`) describes the cell as `ci=false`, which disagrees with the manifest line; the manifest is what the harness reads.
- T2 (covers F2): `ADJ_OFFSET_SS_READ` returns the same snapshot as T1 with `offset` 0.
- T3 (covers F3, F5, F6, F7, F8): each of `ADJ_OFFSET_SINGLESHOT`, `ADJ_OFFSET`, `ADJ_FREQUENCY`, `ADJ_MAXERROR`, `ADJ_ESTERROR`, `ADJ_STATUS`, `ADJ_TIMECONST`, `ADJ_TAI`, `ADJ_TICK`, `ADJ_SETOFFSET`, `ADJ_MICRO` and `ADJ_NANO` returns `EPERM`, and a following query is unchanged. `tests/c/adjtimex_deterministic.c:25` covers only `ADJ_OFFSET`.
- T4 (covers F4): `modes = 0x8000` must return `EINVAL`, `modes = 0xa003` must return the T1 snapshot as a query, and `modes = ADJ_OFFSET_SS_READ | ADJ_SETOFFSET` must return `EPERM`, as on Linux (owner decision 6). Hermit at f4055eb9cae4 returns `EPERM` for all three, so the first two fail until the mode checks follow the kernel's order.
- T5 (covers F9): a NULL `buf` and an unmapped `buf` both give `EFAULT`.
- T6 (covers F1): record and replay a guest that queries `adjtimex`. By the code the recording stops with an unsupported-syscall error; confirm that, and turn it into a test of whatever record/replay policy is chosen.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-09-28 · hermit 81ca8822f78c · hermit2 · closed under the legacy rule (an explicit `classify_syscall` arm), not the v1 done rule; closing comment https://github.com/rrnewton/hermit/issues/333#issuecomment-5872935382
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant A2 of https://github.com/rrnewton/hermit/issues/333: linux_determinism nondeterministic (from **Classification:**); target_policy from legacy NEEDS-HANDLER; an agent chooses model, emulate or refuse; code at this SHA: determinized, dispatch typed.
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · reopen: under the owner rule of 2026-10-02, closed means every flag and option is covered by a named test and the implementation works; adjtimex records no test coverage, so tracking_status is todo and https://github.com/rrnewton/hermit/issues/333 reopens.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy model (investigate -> model): under virtual time Detcore answers modes 0 and ADJ_OFFSET_SS_READ with a fixed STA_UNSYNC snapshot of the virtual clock and refuses every other mode with EPERM; without virtual time the call reaches the unsupported-syscall path; linux_determinism stays nondeterministic; 9 flag behaviours and 6 tests listed; no coverage token yet, so tracking_status stays todo
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d615 (owner decisions 6-15): decision 6 applied (decision 13 needs nothing more: the query already reports virtual time): target is Linux's timekeeping_validate_timex order, so 0x8000 gets EINVAL and 0xa003 is a query; F4 narrowed for the ADJ_SETOFFSET exception; T4 now asserts Linux's answers; target_policy stays model; open question on F4 removed

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `adjtimex`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+adjtimex&body=Discussion+for+the+%60adjtimex%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fadjtimex.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60adjtimex%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: adjtimex" (https://github.com/rrnewton/hermit/issues/333), legacy variant A2. It was ranked 54 in the batch-2 classification order, which excluded the 50 syscalls seen in batch-1 workloads.

The current handler came from https://github.com/rrnewton/hermit/pull/857 ("Determinize clock discipline and kernel log"), which also added `clock_adjtime` and `syslog`; the code still carries its `TODO-HUMAN-REVIEW(PR-857)` tags.

Comment history: 1 comment(s) on https://github.com/rrnewton/hermit/issues/333.

### Open questions

- Should the modeled snapshot report Linux's unsynchronized defaults for `maxerror`, `esterror`, `tolerance`, `precision` and `constant` instead of zeros?
- Record/replay: a recording that reaches `adjtimex` stops with an unsupported-syscall error, because record/replay does not virtualize time. Recording the host answer as `clock_gettime` does, or serving the modeled answer, are both possible; that is an owner policy choice.

### 2026-07-24 host strace evidence

Test host: Linux 6.17.13 x86_64; strace 6.12.

Ran a syscall-specific safe raw probe with deliberately inert/invalid arguments so privileged or mutating operations could not change host state:

```sh
strace -f -qq -e trace=adjtimex -s 160 python3 -c 'import ctypes; c=ctypes.CDLL(None,use_errno=True); c.syscall(159, *([ctypes.c_long(-1)]*6))'
```

Actual strace output:

```text
adjtimex(0xffffffffffffffff)    = -1 EFAULT (Bad address)
```

The trace confirms the host kernel path and observed errno/result for this ABI. The determinism recommendation also accounts for documented success-path state dependencies; an invalid-argument result alone is not treated as proof of determinism.

### Legacy closure

Closed 2026-09-28 by the 2026-09-28 sweep, which closed issues whose syscall had an explicit `classify_syscall` arm. Closing comment: https://github.com/rrnewton/hermit/issues/333#issuecomment-5872935382
