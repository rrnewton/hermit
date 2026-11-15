---
schema: syscall-pages/v1
syscall: sched_getscheduler
nr_x86_64: 145
linux_determinism: conditional
target_policy: model
tracking_status: todo
cross_host_variant: false
related:
  - https://github.com/rrnewton/hermit/issues/415
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+sched_getscheduler&body=Discussion+for+the+%60sched_getscheduler%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fsched_getscheduler.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60sched_getscheduler%60%2C+or+in+what+the+page+says%3F%0A
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

Returns the Linux scheduling policy of a thread. Hermit returns SCHED_OTHER (0) for every call without contacting the kernel. The target keeps that fixed answer and adds Linux's pid validation.

## Linux semantics

`sched_getscheduler(pid)` in Linux 6.9 (kernel/sched/core.c):

- `pid < 0` gives EINVAL.
- `pid` 0 means the calling thread. Otherwise the kernel looks up any live task with that thread id, and an unknown id gives ESRCH.
- On success it returns the policy number: SCHED_OTHER 0, FIFO 1, RR 2, BATCH 3, IDLE 5, DEADLINE 6, or EXT 7 on kernels with sched_ext. If the thread has the reset-on-fork flag set, the result has SCHED_RESET_ON_FORK (0x40000000) ORed in.
- It writes no memory.

### Flags and options

- F1: `pid` selection. 0 is the calling thread; a positive value names any live thread, including a
  non-leader thread; a negative value gives EINVAL; an id that names no live task gives ESRCH.
- F2: the returned value. It is the thread's policy number, ORed with SCHED_RESET_ON_FORK (0x40000000)
  when that flag is set. A thread inherits both from its parent at fork, and both survive execve.

## Determinism analysis

On Linux the answer is the thread's real policy. That value is inherited, so it is whatever the launcher had, unless the guest changed it. Hermit launched under `chrt -b`, under a systemd unit with CPUSchedulingPolicy, or by a parent with reset-on-fork set, would show that host setting to the guest. ESRCH depends on whether the host's task table holds the id. Within one launch environment, a query of the calling thread repeats. linux_determinism stays conditional.

### Nondeterminism sources

- Inherited host scheduling policy and the reset-on-fork flag (F2).
- The host task table, for ESRCH (F1).

## Hermit design

**What the code does today (hermit f4055eb9cae4).**

- detcore/src/lib.rs:2654-2655 is commented "Report the fixed default policy SCHED_OTHER (0)" and returns `Ok(0)` for every call.
- The surrounding block comment (detcore/src/lib.rs:2638-2644) and the determinism argument at detcore/src/syscall_classification.rs:318-325 explain why: Detcore replaces the Linux scheduler, so the Linux policy cannot affect guest-visible computation. A fixed answer removes the inherited host value. The call is listed as determinized at detcore/src/syscall_classification.rs:336.
- The answer agrees with the other policy getters. `sched_getattr` writes SCHED_OTHER (detcore/src/syscalls/threads.rs:2242), the value named `VIRTUAL_CURRENT_POLICY` at detcore/src/syscalls/threads.rs:107-112. `sched_getparam` writes priority 0 (detcore/src/syscalls/threads.rs:2176-2193).

**Gap.** The arm ignores `pid`. `pid = -1` returns 0 where Linux gives EINVAL, and a thread id that names no task returns 0 where Linux gives ESRCH.

**p3 plan: emulate** (the Target paragraph at the end of this section replaces it under owner decision 7). Keep the fixed SCHED_OTHER answer, and add Linux's checks in kernel order:

1. `pid < 0` gives EINVAL.
2. A nonzero `pid` that names no live Detcore thread gives ESRCH. Use the scheduler's own task table, as `handle_sched_setattr` does at detcore/src/syscalls/threads.rs:2423 with `thread_is_live`, not the host's.

Why the p3 plan ruled out the alternatives:

- **Not passthrough.** Passing the call to the kernel would show the guest the inherited host policy.
- **Not refuse.** `chrt -p` (the `chrt` row at tests/e2e/manifests/compat.yaml:185) and runtimes that inspect their own policy need an answer.
- **Not model.** Modelling would mean reporting what the guest set through `sched_setscheduler` or `sched_setattr`, including SCHED_RESET_ON_FORK. That depends on the open question recorded in the sched_setscheduler body. Today detcore/tests/misc/mod.rs:313-315 pins the fixed answer: SCHED_OTHER even after a successful SCHED_FIFO request.

Target (owner decision 7, with decision 6): return the policy in the target thread's virtual scheduling record, ORed with SCHED_RESET_ON_FORK when the record's flag is set (F2). The record is the per-thread state that `sched_setscheduler` and `sched_setattr` write, kept in Detcore's thread state next to `timer_slack_ns` (`detcore/src/tool_local.rs:1795`). It starts as SCHED_OTHER without the flag, whatever the launcher's real policy, so T2's launcher check still holds. A fork or clone child copies the record (as at `detcore/src/lib.rs:1555`), except that a set reset-on-fork flag gives the child SCHED_OTHER with the flag clear (kernel `sched_fork()`); `execve` keeps it. The checks follow the kernel's order (sched_getscheduler(2); kernel `sys_sched_getscheduler()`): `pid < 0` gives EINVAL, then a `pid` with no live guest thread gives ESRCH from `thread_is_live` (`detcore/src/syscalls/threads.rs:2423`) (F1). The assertion at detcore/tests/misc/mod.rs:315 changes to expect SCHED_FIFO. Today the arm returns 0 for every call (`detcore/src/lib.rs:2655`).

## Recommended tests

- T1 (covers F1): under `hermit run --verify` on ptrace: `sched_getscheduler(-1)` gives EINVAL; a thread id
  that never existed gives ESRCH; `sched_getscheduler(gettid())` of a live non-leader thread and
  `sched_getscheduler(0)` both return 0.
- T2 (covers F2): launch the guest from a parent whose real policy is SCHED_BATCH, set with `chrt -b 0`,
  which needs no privilege. The guest must still see 0 from `sched_getscheduler(0)`, so the host policy does
  not leak. The `sched_getscheduler(0) == SCHED_OTHER` assertion in detcore/tests/misc/mod.rs:315
  (`timer_slack_prctl_and_procfs_share_virtual_state`) pins the p3 fixed answer after a SCHED_FIFO
  request; under the target it expects SCHED_FIFO instead (T3).
- T3 (covers F1, F2): read-back under `hermit run --verify` on ptrace. After `sched_setscheduler(0, SCHED_FIFO, {1})`,
  `sched_getscheduler(0)` returns SCHED_FIFO, and a sibling thread's `sched_getscheduler(<that TID>)` returns
  the same while its own `sched_getscheduler(0)` returns SCHED_OTHER. After
  `sched_setscheduler(0, SCHED_OTHER | SCHED_RESET_ON_FORK, &zero)` the answer is 0x40000000. A child forked
  from a SCHED_FIFO parent reports SCHED_FIFO before and after `execve`; a child forked from a
  `SCHED_FIFO | SCHED_RESET_ON_FORK` parent reports SCHED_OTHER. Today Hermit returns 0 in every case.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant A1 of https://github.com/rrnewton/hermit/issues/415: linux_determinism conditional (from **Classification:**); target_policy from legacy NEEDS-HANDLER; an agent chooses model, emulate or refuse; code at this SHA: determinized, dispatch typed.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy investigate -> emulate (fixed SCHED_OTHER per lib.rs:2655, plus pid EINVAL/ESRCH validation that is missing today); linux_determinism conditional kept; 2 flag behaviours and 2 tests; left todo pending the owner question on a modelled read-back policy.
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d615 (owner decisions 6-15): decisions 7 and 6 applied (report the per-thread virtual policy with SCHED_RESET_ON_FORK, fork inheritance per sched_fork; pid EINVAL then ESRCH); target_policy emulate -> model; new T3; T2's stays-0 clause replaced by T3; removed the open question on modelling the read-back

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `sched_getscheduler`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+sched_getscheduler&body=Discussion+for+the+%60sched_getscheduler%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fsched_getscheduler.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60sched_getscheduler%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: sched_getscheduler" (https://github.com/rrnewton/hermit/issues/415), legacy variant A1.

### 2026-07-24 host strace evidence

Evidence gathered on this host (Linux 6.17 x86_64, glibc) with `strace -f -e trace=sched_getscheduler` on a direct `syscall()` harness, invoked with arguments that query, do nothing, or fail, and on common programs where applicable. At that time the call had not yet been classified. At hermit f4055eb9cae4 it is determinized (detcore/src/syscall_classification.rs:336).

Observed strace line:
```
sched_getscheduler(0)           = 0 (SCHED_OTHER)
```

---
*Autonomous syscall-classification sweep of the calls that were then awaiting classification in `syscall_classification.rs`. Host testing + analysis only; no code change proposed here.*
