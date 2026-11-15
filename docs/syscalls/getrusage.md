---
schema: syscall-pages/v1
syscall: getrusage
nr_x86_64: 98
linux_determinism: nondeterministic
target_policy: model
tracking_status: todo
cross_host_variant: false
related: []
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+getrusage&body=Discussion+for+the+%60getrusage%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fgetrusage.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60getrusage%60%2C+or+in+what+the+page+says%3F%0A
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

`getrusage(who, usage)` reports resource usage for the calling process, the calling thread, or its waited-for children. Detcore builds the result itself without a host call (`handle_getrusage`, `detcore/src/syscalls/sysinfo.rs:313`). CPU times come from the same logical CPU accounting as `times(2)`, and fault and context-switch counts are zero. The one exception is `ru_maxrss`, which is read from host procfs and is not deterministic.

Manual reference: Linux man-pages getrusage(2).

## Linux semantics

The kernel accepts exactly three `who` values; anything else, including the kernel-internal `RUSAGE_BOTH` (-2), returns `EINVAL` before the buffer is examined. It then fills a `struct rusage` and copies it out; a bad pointer gives `EFAULT`.

The maintained fields are `ru_utime` and `ru_stime` (scheduler CPU accounting, as `timeval`), `ru_maxrss` (peak resident set in KiB), `ru_minflt` and `ru_majflt` (page faults), `ru_inblock` and `ru_oublock` (block I/O), and `ru_nvcsw` and `ru_nivcsw` (voluntary and involuntary context switches). Linux leaves `ru_ixrss`, `ru_idrss`, `ru_isrss`, `ru_nswap`, `ru_msgsnd`, `ru_msgrcv` and `ru_nsignals` at zero.

For `RUSAGE_CHILDREN`, the counts include every descendant that has terminated and been waited for, including grandchildren that a child waited for. `ru_maxrss` is the peak of the largest such child, not a sum. Children are not counted when `SIGCHLD` is set to `SIG_IGN`, because they are reaped automatically.

### Flags and options

- F1: `RUSAGE_SELF` (0): usage of the whole calling process, summed over all its threads
- F2: `RUSAGE_CHILDREN` (-1): usage of all terminated and waited-for descendants; nothing is counted before the wait, or for children reaped automatically because `SIGCHLD` is `SIG_IGN`
- F3: `RUSAGE_THREAD` (1): usage of the calling thread only (Linux-specific)
- F4: any other `who`, including `RUSAGE_BOTH` (-2): `EINVAL`, checked before `usage`
- F5: `usage` NULL or not writable: `EFAULT`
- F6: `ru_maxrss`: peak resident set size in KiB; for `RUSAGE_CHILDREN`, the largest waited-for descendant
- F7: counter fields `ru_minflt`, `ru_majflt`, `ru_inblock`, `ru_oublock`, `ru_nvcsw`, `ru_nivcsw`, and the unmaintained fields that Linux always reports as zero

## Determinism analysis

`linux_determinism: nondeterministic` (changed from the rendered `conditional`). CPU times reflect real scheduler accounting and host load, page-fault and block-I/O counts depend on host memory and page cache state, context-switch counts depend on scheduling, and `ru_maxrss` depends on host allocation and page-in behaviour. No successful call repeats exactly across runs; only the error cases (F4, F5) do.

### Nondeterminism sources

- Host CPU speed, load and scheduler accounting (`ru_utime`, `ru_stime`)
- Host scheduling decisions (`ru_nvcsw`, `ru_nivcsw`)
- Host page cache and memory pressure (`ru_minflt`, `ru_majflt`, `ru_inblock`, `ru_oublock`, `ru_maxrss`)
- When, and whether, children are waited for (F2)

## Hermit design

Target policy: `model` (changed from `investigate`). The guest sees CPU times derived from logical time, zero counters, and (intended) a deterministic peak RSS. The handler at `detcore/src/syscalls/sysinfo.rs:313` makes no host call:

- `who` outside the three accepted values gives `EINVAL` (`detcore/src/syscalls/sysinfo.rs:321`) before the NULL check gives `EFAULT` (`detcore/src/syscalls/sysinfo.rs:324`), matching Linux's order (F4, F5). A non-NULL bad pointer gets `EFAULT` from the guest memory write (`detcore/src/syscalls/sysinfo.rs:351`).
- `RUSAGE_THREAD` reads the calling thread's own logical CPU counters, `RUSAGE_SELF` the process totals, and `RUSAGE_CHILDREN` the reaped-children totals (`detcore/src/syscalls/sysinfo.rs:329`). The doc comment at `detcore/src/syscalls/sysinfo.rs:288` explains that these share their source with `times(2)`. Times are truncated to microseconds (`timeval_from_logical`, `detcore/src/syscalls/sysinfo.rs:43`).
- Child totals are added only when a child is reaped (`reap_child`, `detcore/src/tool_local.rs:1630`), and they include the child's own reaped children, as on Linux (F2).
- Every counter field stays zero (F7); the doc comment says Detcore has no model for them and will not synthesize one.

What remains before the code fully realizes `model`, from reading the code and not measured:

- **`ru_maxrss` comes from the host (F6).** For `RUSAGE_SELF` and `RUSAGE_THREAD`, the handler reads `VmHWM` (falling back to `VmRSS`) from host procfs for the guest's pid and floors the result at 1 (`guest_peak_rss_kb`, `detcore/src/syscalls/sysinfo.rs:390`; set at `detcore/src/syscalls/sysinfo.rs:347`). The value depends on host memory behaviour, is not recorded, and can differ between the two halves of `--verify` and between record and replay. The fixture `tests/c/getrusage_self_accounting.c:16` deliberately does not assert it, saying it differs per backend.
- **`ru_maxrss` for `RUSAGE_CHILDREN` is always 0 (F6).** The comment at `detcore/src/syscalls/sysinfo.rs:344` says there is no child accounting for it. Linux reports the largest waited-for child's peak, so a program that reads it after a wait sees 0 under Hermit and a positive value natively.
- **Thread CPU time is a different abstraction.** Linux's CPU times measure real execution; Detcore's measure logical instructions and syscall time. That is the model, not a gap, but it means absolute values never match native runs.

`passthrough` would expose host scheduler counters, and `refuse` would break shells, build tools and benchmarks that report usage, so neither is an alternative.

## Recommended tests

- T1 (covers F1, F3, F7): `RUSAGE_SELF` and `RUSAGE_THREAD` succeed, CPU times advance across syscall work, and every counter and unmaintained field reads as zero, identically across `hermit run --verify` on ptrace.
  Existing candidates: `check_self_and_thread_rusage_advances` in `tests/c/resource_determinism.c:390` (cell `c-programs/resource-determinism`, `tests/e2e/manifests/c-programs.yaml:7549`, verify ci on ptrace), whose `read_rusage` (`tests/c/resource_determinism.c:316`) poisons the buffer with `0xa5` and checks every byte outside the modelled fields for zero; and `tests/c/getrusage_self_accounting.c` (cell at `tests/e2e/manifests/c-programs.yaml:12010`, verify ci on ptrace), which checks that `RUSAGE_SELF` agrees with `times(2)` and that the counters are zero.
- T2 (covers F2): `RUSAGE_CHILDREN` CPU times do not change while a child has exited but not been waited for, and include the child after the wait. Add a grandchild case and a `SIGCHLD`-ignored case.
  Existing candidate: the fork section of `check_times` in `tests/c/resource_determinism.c:583`, which uses `waitid(..., WNOWAIT)` to hold the child unreaped; it has neither extra case. Unit test `child_cpu_time_is_hidden_until_reap` at `detcore/src/tool_local.rs:3452`.
- T3 (covers F4, F5): `who` 1234 and `RUSAGE_BOTH` (-2) return `EINVAL`, also with a NULL buffer; a NULL and an unmapped buffer return `EFAULT`.
  Existing candidate: `check_rusage_errors` in `tests/c/resource_determinism.c:416` covers who 1234 and a NULL buffer only.
- T4 (covers F6): print the raw `ru_maxrss` for `RUSAGE_SELF` under `hermit run --verify` and under record/replay, and check that it is positive and identical in both runs. Natively it is positive for `RUSAGE_CHILDREN` after a wait; record what Hermit returns. The current `read_rusage` check requires `ru_maxrss == 0` for `RUSAGE_CHILDREN` after a reap (`tests/c/resource_determinism.c:636`), which pins the difference from Linux.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · created for v1: no tracking issue existed for this pinned Sysno; code at this SHA: determinized, dispatch typed.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy investigate -> model (logical CPU times shared with times(2), zero counters, no host call); linux_determinism conditional -> nondeterministic (scheduler, fault and RSS accounting); 7 flag behaviours and 4 tests listed; open question: ru_maxrss is read from host procfs and is 0 for RUSAGE_CHILDREN; tracking_status stays todo

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `getrusage`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+getrusage&body=Discussion+for+the+%60getrusage%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fgetrusage.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60getrusage%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Created by the syscall-tracking v1 migration on 2026-10-03. No tracking issue existed for `getrusage`: the 2026-07-24 classification campaign filed issues only for syscalls in the old temporary pass-through block, and Detcore already handled this one.

### Open questions

- What should the deterministic model for `ru_maxrss` be? Today it is a host procfs reading (`detcore/src/syscalls/sysinfo.rs:390`) that is neither recorded nor reproducible. The handler's doc comment says rr's `rusage` test requires a positive value, so a constant would satisfy that but carry no information; a value derived from Detcore's own view of guest mappings would be more faithful. This is an owner policy choice.
- Should `RUSAGE_CHILDREN` report the largest reaped child's `ru_maxrss`, as Linux does, instead of 0? That depends on the answer above, and changing it means updating the zero check in `tests/c/resource_determinism.c:636`.
- Is a child reaped automatically because `SIGCHLD` is `SIG_IGN` kept out of the children totals, as on Linux? This body has not checked the auto-reap path.
