---
schema: syscall-pages/v1
syscall: times
nr_x86_64: 100
linux_determinism: nondeterministic
target_policy: model
tracking_status: todo
cross_host_variant: false
related:
  - https://github.com/rrnewton/hermit/issues/498
  - https://github.com/rrnewton/hermit/issues/519
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+times&body=Discussion+for+the+%60times%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Ftimes.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60times%60%2C+or+in+what+the+page+says%3F%0A
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

`times(buf)` returns the CPU time used by the calling process and by its waited-for children, and returns an elapsed clock-tick count as its result. Detcore answers without a host call (`handle_times`, `detcore/src/syscalls/sysinfo.rs:363`): the CPU fields come from logical CPU accounting shared with `getrusage(2)`, and the result counts logical time since the epoch plus a fixed boot offset.

Manual reference: Linux man-pages times(2), https://man7.org/linux/man-pages/man2/times.2.html.

## Linux semantics

All values are in clock ticks of `sysconf(_SC_CLK_TCK)`, which is `USER_HZ`, 100 on x86_64. If `buf` is non-NULL, the kernel fills `struct tms`:

- `tms_utime` and `tms_stime`: user and system CPU time of the whole process, summed over all threads
- `tms_cutime` and `tms_cstime`: the totals of every child that has terminated and been waited for, including that child's own waited-for descendants

The result is the number of ticks elapsed since an arbitrary point in the past. Since Linux 2.6 that point is (2^32/HZ) - 300 seconds before boot, where HZ is the kernel's internal tick rate, so the value depends on boot time and on the kernel's configuration. The two host probes under Additional information returned 439098885 and 439105634. `buf` may be NULL, in which case only the result is returned. A bad `buf` gives `EFAULT`.

### Flags and options

- F1: `buf` non-NULL: the four `struct tms` fields are filled with process and child CPU ticks
- F2: `buf` NULL: accepted; nothing is written and the elapsed-tick result is still returned
- F3: `buf` not writable: `EFAULT`
- F4: the result: clock ticks since an arbitrary point that Linux places (2^32/HZ) - 300 seconds before boot
- F5: `tms_cutime` and `tms_cstime` count only children that have terminated and been waited for, including their waited-for descendants

## Determinism analysis

`linux_determinism: nondeterministic`, kept from the legacy classification of both https://github.com/rrnewton/hermit/issues/498 and https://github.com/rrnewton/hermit/issues/519. The CPU fields measure real CPU consumption, which varies from run to run with host load, CPU speed and scheduling. The result encodes the host's boot time and the moment of the call. No successful call repeats.

### Nondeterminism sources

- Host load, CPU speed and scheduler accounting (F1)
- The kernel's tick frequency and accounting granularity
- Host boot time and the moment of the call (F4)
- When, and whether, children run and are waited for, and how much CPU they use (F5)

## Hermit design

Target policy: `model` (changed from `investigate`; legacy recommended action **NEEDS-HANDLER** in both issues). The guest sees CPU and elapsed ticks derived from Detcore's logical time. The code at this SHA realizes it with no host call. The handler carries `AUTONOMOUS-BOT-IMPLEMENTED` and `TODO-HUMAN-REVIEW(#797)` tags (`detcore/src/syscalls/sysinfo.rs:355`; https://github.com/rrnewton/hermit/issues/797).

- Typed dispatch at `detcore/src/lib.rs:2774` sends every call to `handle_times`.
- F4: the result is `logical_clock_ticks` (`detcore/src/syscalls/sysinfo.rs:51`): `--sysinfo-uptime-offset` (default 120 seconds) times 100, plus the logical time elapsed since the epoch in 10 ms ticks, with wrapping arithmetic like Linux's `clock_t` (`detcore/src/syscalls/sysinfo.rs:370`). It starts at 12000 ticks in every run, where Linux starts near 429 million on a host with HZ 1000. It shares its offset with `sysinfo` uptime, so the two agree.
- F1, F5: the `tms` fields come from the process's logical CPU snapshot: user and system time summed over its threads, and child totals that change only when a child is reaped (`reap_child`, `detcore/src/tool_local.rs:1630`, which also adds the child's own reaped children). They are written only when `buf` is non-NULL (`detcore/src/syscalls/sysinfo.rs:373`).
- F2: with `buf` NULL the handler skips the write and still returns the tick count (`detcore/src/syscalls/sysinfo.rs:383`).
- F3: a bad non-NULL `buf` gets `EFAULT` from the guest memory write.

Both duplicate issues say Hermit forwarded `times` to the host from the not-yet-classified block of `detcore/src/syscall_classification.rs` (the twin cites `dd60278f`). At this SHA that claim is stale and the code wins: `times` is classified as Determinized (assertion at `detcore/src/syscall_classification.rs:1953`) and handled by `handle_times`.

`passthrough` would expose host CPU accounting and boot time, and `refuse` would break shells and build tools that time their children, so neither is an alternative.

## Recommended tests

- T1 (covers F1, F4): two `times(&buf)` calls around syscall work show `tms_stime` and the result advancing, and the printed values are identical across `hermit run --verify` on ptrace.
  Existing candidate: `check_times` in `tests/c/resource_determinism.c:554` (cell `c-programs/resource-determinism`, `tests/e2e/manifests/c-programs.yaml:7549`, verify ci on ptrace). Unit tests `logical_clock_ticks_include_boot_offset_and_fractional_seconds` at `detcore/src/syscalls/sysinfo.rs:506` and `logical_clock_ticks_wrap_configured_offset_like_linux_clock_t` at `detcore/src/syscalls/sysinfo.rs:933`.
- T2 (covers F2, F3): `times(NULL)` succeeds and returns a tick count no smaller than the previous one; `syscall(SYS_times, (void *)1)` returns `EFAULT`.
  Existing candidate: `tests/c/resource_determinism.c:646` and `tests/c/resource_determinism.c:653`.
- T3 (covers F5): `tms_cstime` does not change while a child has exited but not been waited for, and includes the child after the wait. Add a grandchild case.
  Existing candidate: the fork section of `check_times`, which holds the child with `waitid(..., WNOWAIT)` before reaping it (`tests/c/resource_determinism.c:602`); it has no grandchild case. Unit test `child_cpu_time_is_hidden_until_reap` at `detcore/src/tool_local.rs:3452`.
- T4 (covers F1): `times` and `getrusage(RUSAGE_SELF)` report the same logical CPU time to within one tick.
  Existing candidates: `tests/c/getrusage_self_accounting.c` (cell at `tests/e2e/manifests/c-programs.yaml:12010`, verify ci on ptrace) and unit test `rusage_and_times_agree_within_one_clock_tick` at `detcore/src/syscalls/sysinfo.rs:910`.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant A1 of https://github.com/rrnewton/hermit/issues/498: linux_determinism nondeterministic (from **Classification:**); target_policy from legacy NEEDS-HANDLER; an agent chooses model, emulate or refuse; code at this SHA: determinized, dispatch typed.
- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · duplicate https://github.com/rrnewton/hermit/issues/519 is retired into this issue; its text waits under Additional information for a section-by-section merge.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy investigate -> model (logical CPU ticks shared with getrusage, elapsed ticks from logical time plus a fixed boot offset, no host call); linux_determinism stays nondeterministic; 5 flag behaviours and 4 tests listed; duplicate https://github.com/rrnewton/hermit/issues/519 merged section by section and the stale temporary pass-through claim reconciled against the code; tracking_status stays todo

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `times`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+times&body=Discussion+for+the+%60times%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Ftimes.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60times%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: times" (https://github.com/rrnewton/hermit/issues/498), legacy variant A1. Duplicate https://github.com/rrnewton/hermit/issues/519, filed the same day with the same title (legacy variant A1), is merged into this body; its comment history (0 comments) stays on that issue.

### 2026-07-24 host strace evidence

Evidence gathered on this host (Linux 6.17 x86_64, glibc) with `strace -f -e trace=times` on a direct `syscall()` harness (invoked with non-destructive query/no-op/failing arguments) and, where applicable, on common programs. At the time, the report said Hermit routed `times` through the old temporary pass-through block in `detcore/src/syscall_classification.rs`; that is no longer true (see Hermit design).

Observed strace line:
```
times({tms_utime=0, tms_stime=53 /* 0.53 s */, tms_cutime=0, tms_cstime=0}) = 439098885
```

From https://github.com/rrnewton/hermit/issues/519: evidence gathered on `the measurement host` (Linux 6.17, x86_64, glibc) with a direct `syscall()` harness, using safe self-targeting, no-op, temporary-file, or deliberately invalid arguments, traced with `strace -qq -f -e trace=times`. The harness exited 0.

```text
times({tms_utime=0, tms_stime=0, tms_cutime=0, tms_cstime=0}) = 439105634
```

Both reports came from the autonomous syscall-classification sweep: ranks 151-250 for https://github.com/rrnewton/hermit/issues/498, and batch 5 (ranks 201-250) for https://github.com/rrnewton/hermit/issues/519. Neither proposed a code change.
