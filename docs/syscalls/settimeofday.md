---
schema: syscall-pages/v1
syscall: settimeofday
nr_x86_64: 164
linux_determinism: nondeterministic
target_policy: refuse
tracking_status: todo
cross_host_variant: false
related:
  - https://github.com/rrnewton/hermit/issues/460
  - https://github.com/rrnewton/hermit/issues/467
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+settimeofday&body=Discussion+for+the+%60settimeofday%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fsettimeofday.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60settimeofday%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 239ca43d3092fe64884782ff3166baf34059e41c
  detcore_class: determinized
  dispatch: "pred:is_mount_ns_admin_refused_syscall"
  refused: true
  refused_only_when_fail_closed: false
  subscribed_run: true
  subscribed_record_replay: true
  tests_naming: 0
---

## Summary

`settimeofday(tv, tz)` sets the system wall clock (`CLOCK_REALTIME`) and, optionally, the kernel's legacy timezone. It is obsoleted by `clock_settime`. Hermit refuses every call with a fixed `EPERM` and never forwards it to the host (`detcore/src/lib.rs:2215-2219` at hermit f4055eb9cae4), the same answer it gives for `clock_settime` (`detcore/src/lib.rs:2477`).

Manual reference: [settimeofday(2)](https://man7.org/linux/man-pages/man2/settimeofday.2.html).

## Linux semantics

The kernel checks the arguments in a fixed order (`kernel/time/time.c`):

1. `tv`, if non-NULL, is copied in (`EFAULT` on a bad pointer) and its microseconds are range-checked (`EINVAL`).
2. `tz`, if non-NULL, is copied in (`EFAULT`).
3. The seconds value must be in the settable range (`EINVAL`).
4. The LSM hook `security_settime64` runs. The capability module requires `CAP_SYS_TIME` in the initial user namespace, so it fails with `EPERM` for any process inside a user namespace, and for an unprivileged host process, even when both pointers are NULL.
5. `tz` is range-checked (`tz_minuteswest` within ±15 hours, else `EINVAL`) and stored in the global `sys_tz`. The first timezone-only call after boot also warps the clock once (`timekeeping_warp_clock`), for hardware clocks kept in local time.
6. If `tv` is non-NULL, the wall clock steps to the new value for every process on the host.

Time namespaces do not virtualize `CLOCK_REALTIME`, so a time namespace offers no private wall clock to set.

The two host probes under Additional information both stop before the capability check: an out-of-range `tv_usec` gave `EINVAL`, and a bad `tv` pointer gave `EFAULT`. Neither changed the clock.

### Flags and options

- F1: `tv` non-NULL and valid: step `CLOCK_REALTIME` for the whole host. Needs `CAP_SYS_TIME` in the initial user namespace; otherwise `EPERM`.
- F2: `tz` non-NULL: set the kernel timezone `sys_tz` (`tz_minuteswest` must be within ±900 minutes, else `EINVAL`; `tz_dsttime` is stored but unused). The first timezone-only call after boot warps the clock once. Same capability requirement as F1.
- F3: `tv` and `tz` both NULL: changes nothing; returns 0 with `CAP_SYS_TIME`, `EPERM` without it.
- F4: invalid `tv`: `tv_usec` outside 0..999999, or `tv_sec` outside the settable range, gives `EINVAL` before the capability check.
- F5: bad `tv` or `tz` pointer: `EFAULT` before the capability check.

## Determinism analysis

`linux_determinism: nondeterministic`, kept from the legacy classification. A successful call changes host-global state: every later wall-clock read by every process on the host, and every file timestamp written afterwards, depends on it, and the host's own time daemons may be adjusting the same clock concurrently. Whether it succeeds depends on the caller's capabilities and LSM policy. The failure paths (F4, F5, and F1-F3 without `CAP_SYS_TIME`) depend only on the arguments and the caller's privilege, so they repeat; the success path never does with respect to the rest of the host.

### Nondeterminism sources

- Capabilities, LSM policy and user-namespace placement decide between success and `EPERM`.
- A successful set is global: it moves the clock seen by processes outside the guest and races with NTP or `chrony` adjustments.
- Hermit's guest sees a virtual wall clock (`gettimeofday` is handled by Detcore when time is virtualized, `detcore/src/lib.rs:2447`), so forwarding a set would either fail or change a clock the guest no longer reads.

## Hermit design

Target policy: `refuse`. At hermit f4055eb9cae4 the code already realizes it:

- `classify_syscall` marks `settimeofday` Determinized in the mount and namespace administration group (`detcore/src/syscall_classification.rs:339-369`, reviewed under the code tag `TODO-HUMAN-REVIEW(#724)`, https://github.com/rrnewton/hermit/issues/724).
- `is_mount_ns_admin_refused_syscall` (`detcore/src/syscall_classification.rs:939`) includes it, and `handle_syscall_event` returns `Err(Errno::EPERM)` for every member before the typed match (`detcore/src/lib.rs:2215-2219`). The arm is unconditional: it does not depend on `--strict`/`panic_on_unsupported_syscalls` or on whether time is virtualized.
- The same predicate feeds `is_deterministically_refused_syscall` (`detcore/src/syscall_classification.rs:1324`), so `code_facts.refused` is true.
- The only test is the unit test `mount_ns_admin_syscalls_are_determinized_and_consistent` (`detcore/src/syscall_classification.rs:2666`). It checks the classification and the predicate's membership; it runs no guest.

Why `refuse`:

- `EPERM` is what Linux itself returns to the Hermit guest for valid arguments. The ptrace guest runs inside a user namespace (`detcore/src/syscalls/misc.rs:672`), and `CAP_SYS_TIME` is checked against the initial user namespace, so a forwarded call could never succeed there.
- It is consistent with the rest of the clock-setting family: `clock_settime` returns a fixed `EPERM` (`detcore/src/lib.rs:2477`), and `adjtimex`/`clock_adjtime` refuse their mutation modes (`detcore/src/syscall_classification.rs:613-615`).
- `model` (moving the guest's virtual wall clock by the requested offset) was the other candidate the legacy NEEDS-HANDLER action named. It would let a guest that runs as virtual root set its own clock deterministically, but no workload in the tree needs it, and it would make `settimeofday` succeed where `clock_settime` refuses. Choosing it should be done for the whole family at once.
- `passthrough`, the legacy behaviour, could only fail or, if Hermit ever held `CAP_SYS_TIME`, change the host clock.

Known difference from Linux: Hermit returns `EPERM` before looking at the arguments, so F4 and F5 give `EPERM` under Hermit where Linux gives `EINVAL` or `EFAULT`. The result is still deterministic.

Target (owner decision 6): keep the fixed `EPERM`, returned before either pointer is read, which is what the code does today (`detcore/src/lib.rs:2215-2219`). Linux's order is the one listed under Linux semantics (`SYSCALL_DEFINE2(settimeofday)` and `do_sys_settimeofday64` in `kernel/time/time.c`: copy and range checks, then `security_settime64`); Hermit deliberately does not reproduce the `EINVAL` and `EFAULT` that come before the capability check. For valid arguments the fixed `EPERM` is exactly what Linux gives a process in a user namespace, so `date -s` and NTP clients behave as they do in an unprivileged container. Setting a private virtual wall clock (`model`) stays out of scope: owner decision 9 models a refused family only where a named workload breaks, and none is known here; owner decision 13 moves clock and timer reads to virtual time and does not add a settable clock.

## Recommended tests

- T1 (covers F1, F2, F3): under `hermit run --verify` on ptrace, call `syscall(SYS_settimeofday, ...)` with (valid `tv`, NULL), (NULL, valid `tz`), (valid `tv`, valid `tz`) and (NULL, NULL). Each must return -1 with `errno == EPERM`. A `clock_gettime(CLOCK_REALTIME)` and a `gettimeofday` taken before and after must show the virtual clock continuing, with no jump to the requested time.
- T2 (covers F4, F5): call it with `tv_usec = 1000000`, `tz_minuteswest = 10000`, and `tv = (void *)-1`. Each returns the fixed `EPERM` of owner decision 6 (Linux gives `EINVAL`, `EINVAL` and `EFAULT`, the second only to a caller holding `CAP_SYS_TIME`), and both `--verify` runs agree.
- T3 (covers F1): `hermit record` then `hermit replay` a guest that calls `settimeofday` and prints the return value and `errno`. Replay must print the same `EPERM` result.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant A1 of https://github.com/rrnewton/hermit/issues/460: linux_determinism nondeterministic (from **Classification:**); target_policy from legacy NEEDS-HANDLER; an agent chooses model, emulate or refuse; code at this SHA: determinized, dispatch pred:is_mount_ns_admin_refused_syscall, refused.
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · duplicate https://github.com/rrnewton/hermit/issues/467 is retired into this issue; its text waits under Additional information for a section-by-section merge.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy investigate -> refuse (Detcore returns a fixed EPERM at detcore/src/lib.rs:2215 without forwarding, matching clock_settime at detcore/src/lib.rs:2477); linux_determinism stays nondeterministic; duplicate https://github.com/rrnewton/hermit/issues/467 merged; 5 flag behaviours and 3 tests listed; no coverage token because no test runs a guest that calls settimeofday, so tracking_status stays todo; open question on errno order under Additional information
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d615 (owner decisions 6-15): decision 6 applied (fixed EPERM before reading tv/tz stays the target; Linux order in kernel/time/time.c deliberately not reproduced); decision 9 with 13: no settable virtual wall clock; target_policy stays refuse; T2 now asserts the decision-6 errno; no new F/T ids; both open questions removed

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `settimeofday`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+settimeofday&body=Discussion+for+the+%60settimeofday%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fsettimeofday.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60settimeofday%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: settimeofday" (https://github.com/rrnewton/hermit/issues/460), legacy variant A1. Its comment history stays on that issue.

Merged duplicate: https://github.com/rrnewton/hermit/issues/467, filed 2026-07-24 as "Syscall classification: settimeofday" (legacy variant A1), retired into this issue. Its comment history (0 comments at migration) stays on that issue.

### Merge notes

- Both issues classified the call NONDETERMINISTIC and recommended NEEDS-HANDLER; the merged body keeps `nondeterministic` and resolves NEEDS-HANDLER as `refuse`.
- The duplicate's facts kept here: it is obsoleted by `clock_settime`; it requires `CAP_SYS_TIME`; it must be neutralized so it cannot affect Hermit's virtual time; a bad pointer fails with `EFAULT` (now F5).
- Both issues said Hermit routed `settimeofday` through the UNCLASSIFIED temporary pass-through block of `detcore/src/syscall_classification.rs` (at `dd60278f`). That was true when filed and is superseded: at f4055eb9cae4 the call is Determinized and refused (Hermit design). The code at the cited SHA wins.
- No other contradiction: the two strace probes test different argument errors (F4 and F5).

### 2026-07-24 host strace evidence (https://github.com/rrnewton/hermit/issues/460)

Evidence was gathered on `the measurement host` (Linux 6.17, x86_64, glibc) with a direct `syscall()` harness. The probe used safe self-targeting, no-op, temporary-file, or deliberately invalid arguments and was traced with `strace -qq -f -e trace=settimeofday`. The harness exited 0, and `detcore/src/syscall_classification.rs` at `dd60278f` places `settimeofday` in the UNCLASSIFIED temporary-passthrough block.

Observed strace line:
```text
settimeofday({tv_sec=0, tv_usec=1000000}, NULL) = -1 EINVAL (Invalid argument)
```

---
*Autonomous syscall-classification sweep, batch 5 (ranks 201-250 of the unique `syscall_classification.rs` UNCLASSIFIED list). Manual research plus host strace evidence; no code change proposed here.*

### 2026-07-24 host strace evidence (duplicate https://github.com/rrnewton/hermit/issues/467)

Evidence gathered on this host (Linux 6.17 x86_64, glibc) with `strace -f -e trace=settimeofday` on a direct `syscall()` harness (invoked with non-destructive query/no-op/failing arguments) and, where applicable, on common programs. Hermit then routed `settimeofday` through the UNCLASSIFIED temporary-passthrough block in `detcore/src/syscall_classification.rs`.

Observed strace line:
```
settimeofday(0xffffffffffffffff, NULL) = -1 EFAULT (Bad address)
```

---
*Autonomous syscall-classification sweep (ranks 151-250 of the `syscall_classification.rs` UNCLASSIFIED list). Host testing + analysis only; no code change proposed here.*
