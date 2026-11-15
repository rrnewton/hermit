---
schema: syscall-pages/v1
syscall: sched_setscheduler
nr_x86_64: 144
linux_determinism: conditional
target_policy: model
tracking_status: todo
cross_host_variant: false
related:
  - https://github.com/rrnewton/hermit/issues/419
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+sched_setscheduler&body=Discussion+for+the+%60sched_setscheduler%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fsched_setscheduler.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60sched_setscheduler%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 239ca43d3092fe64884782ff3166baf34059e41c
  detcore_class: determinized
  dispatch: "typed"
  refused: false
  refused_only_when_fail_closed: false
  subscribed_run: true
  subscribed_record_replay: true
  tests_naming: 0
---

## Summary

Sets both the Linux scheduling policy and the static priority of a thread. Hermit replaces the Linux scheduler with Detcore's, so it suppresses the request and returns 0 for every argument, including arguments Linux rejects. The target is the same suppression, with Linux's argument validation added in kernel order, which `sched_setattr` already does.

## Linux semantics

`sched_setscheduler(pid, policy, param)` combines a policy change with a `struct sched_param` (one `int sched_priority`). The checks run in this order, and when two arguments are wrong at once the first failing check decides the errno (Linux 6.9, kernel/sched/core.c):

1. `policy < 0` gives EINVAL, before anything else.
2. A NULL `param` or `pid < 0` gives EINVAL. A NULL `param` is EINVAL, not EFAULT.
3. Copying `param` from an unreadable address gives EFAULT.
4. `pid` 0 means the calling thread. Otherwise the kernel looks up any live task with that thread id, not only a thread-group leader, and an unknown id gives ESRCH.
5. Only now is the policy itself judged: unknown policy, priority range, real-time versus non-real-time priority, deadline parameters. Last come the credential and rlimit checks, which give EPERM.

On success the thread's real policy and priority change and the call returns 0. The change affects host timing only. It returns no data.

### Flags and options

- F1: `pid` selection. 0 is the calling thread; a positive value names any live thread, including a
  non-leader thread; a negative value gives EINVAL; an id that names no live task gives ESRCH.
- F2: `param` pointer. NULL gives EINVAL, and an unreadable address gives EFAULT. Both are checked before
  the pid lookup.
- F3: non-real-time policies SCHED_OTHER (0), SCHED_BATCH (3) and SCHED_IDLE (5). `sched_priority` must
  be 0, otherwise EINVAL. The thread's nice value is kept.
- F4: real-time policies SCHED_FIFO (1) and SCHED_RR (2). `sched_priority` must be 1..99: 0 or a value
  above 99 gives EINVAL. Without CAP_SYS_NICE, a priority above RLIMIT_RTPRIO gives EPERM.
- F5: SCHED_DEADLINE (6). This call cannot pass runtime, deadline or period, so the kernel's deadline
  parameter check always fails with EINVAL. `sched_setattr` is the only way to select SCHED_DEADLINE.
- F6: the SCHED_RESET_ON_FORK modifier (0x40000000), ORed into `policy`. Children created afterwards
  start with SCHED_OTHER, and with nice 0 if the parent's nice was negative. Clearing the flag once it is
  set needs CAP_SYS_NICE, otherwise EPERM.
- F7: invalid policy values: a negative value (EINVAL, step 1), the reserved 4 (SCHED_ISO), and anything
  above the last policy give EINVAL. SCHED_EXT (7) is valid only on kernels built with
  CONFIG_SCHED_CLASS_EXT (6.12 and later); elsewhere it gives EINVAL.
- F8: credential and rlimit checks, all EPERM: targeting a thread owned by another user without
  CAP_SYS_NICE; leaving SCHED_IDLE without RLIMIT_NICE room; the real-time limit in F4.

## Determinism analysis

The return value is a pure function of the arguments, the caller's credentials and rlimits, the host kernel's configuration (SCHED_EXT in F7), and whether the target id names a live task. On one host, running as the same user with the same limits, a valid request repeats. The guest-visible risks are:

- EPERM depends on privilege (CAP_SYS_NICE, RLIMIT_RTPRIO, RLIMIT_NICE). A program that runs as root on one machine and unprivileged on another sees different answers.
- ESRCH depends on the host's process table, unless thread ids are virtualized.
- A real policy change alters host timing, and therefore the interleaving that Hermit's own scheduler must control.

linux_determinism stays conditional: the answer repeats only when credentials, limits and the kernel build are held fixed.

### Nondeterminism sources

- Privilege-dependent EPERM (F4, F6, F8).
- Kernel-build-dependent SCHED_EXT acceptance (F7).
- The host task table, for ESRCH (F1).
- A real policy change would alter host scheduling, so Hermit must never forward it.

## Hermit design

**What the code does today (hermit f4055eb9cae4).**

- The dispatch arm at detcore/src/lib.rs:2652 is `Syscall::SchedSetscheduler(_) => Ok(0)`. It sits in the block commented at detcore/src/lib.rs:2638-2644, which says setters are no-ops and getters return a fixed SCHED_OTHER answer.
- detcore/src/syscall_classification.rs:318-325 gives the determinism argument for that block: Detcore replaces the Linux scheduler, so policy and priority cannot change guest-visible computation. It lists the call as determinized at detcore/src/syscall_classification.rs:334.
- Nothing reaches the kernel and no state is kept.

The arm ignores every argument, so Hermit returns 0 where Linux would return EINVAL or ESRCH. Examples: `pid = -1`, a NULL `param`, policy 4, SCHED_FIFO with priority 0 or 100, SCHED_OTHER with priority 5, SCHED_DEADLINE, and a pid that names no thread. A program that probes for a valid policy, or checks that an error path works, takes the wrong branch. The answer is a constant, so it is deterministic, but it is not Linux's answer.

**p3 plan: emulate** (the Target paragraph at the end of this section replaces it under owner decision 7). Keep suppressing the effect, and add Linux's argument validation in kernel order. This is already the design of the sibling `sched_setattr` handler:

- Its doc comment, detcore/src/syscalls/threads.rs:2257-2317, says a well-formed request is a deterministic no-op "matching the existing sched_setscheduler and sched_setparam policy".
- The same comment says a malformed one must fail as Linux fails, because both directions are guest-visible.
- Under "Deliberately not emulated" (detcore/src/syscalls/threads.rs:2292-2312) it states the rule for privilege: an answer that depends on the host kernel configuration or on the caller's privileges is not reproduced. Real-time EPERM is accepted as a no-op, and SCHED_EXT (7) is accepted unconditionally.

Concretely:

- Check `policy < 0` first (EINVAL), then `pid < 0` or a NULL `param` (EINVAL), then read `param` (EFAULT).
- Resolve a nonzero `pid` against Detcore's own task table, as `handle_sched_setattr` does at detcore/src/syscalls/threads.rs:2423 with `thread_is_live`, which asks the scheduler rather than the host (ESRCH).
- Then strip SCHED_RESET_ON_FORK and judge the policy with the existing `is_valid_sched_policy` (detcore/src/syscalls/threads.rs:235) and `is_rt_policy` (detcore/src/syscalls/threads.rs:248): EINVAL for an invalid policy, a priority out of range, a mismatch between real-time policy and nonzero priority, and SCHED_DEADLINE.
- Return 0 without contacting the kernel.
- Under the rule above, the EPERM cases in F4, F6 and F8 are accepted as the same no-op.

Why the p3 plan chose emulate rather than model or refuse:

- **Not refuse.** The call has no unmodeled effect: suppressing it is faithful to Detcore's single scheduler. Real programs probe it (the `chrt` row in tests/e2e/manifests/compat.yaml:185 exercises the scheduler-policy family), and refusing would break them.
- **Not model, yet.** Today nothing reads the policy back. `sched_getscheduler` returns a fixed SCHED_OTHER (detcore/src/lib.rs:2655), and `sched_getattr` writes a fixed SCHED_OTHER (detcore/src/syscalls/threads.rs:2242), the value the constant `VIRTUAL_CURRENT_POLICY` names (detcore/src/syscalls/threads.rs:107-112).
- **The timer slack.** The PR_SET_TIMERSLACK handler relies on that fixed policy: detcore/src/syscalls/misc.rs:384-385 says the kernel's real-time and deadline timer-slack branch is unreachable because the policy is virtualized as SCHED_OTHER.
- **An existing test pins the current behaviour.** detcore/tests/misc/mod.rs:313-315 sets SCHED_FIFO, expects 0, and then expects `sched_getscheduler(0)` to report SCHED_OTHER.
- The `getpriority` sample chose to model a per-thread virtual value with read-back instead. Owner decision 7 (2026-10-04) settled the question this bullet raised: model the read-back. The validation work above is needed under either answer.

Target (owner decision 7, with decision 6): the validation above stays in the kernel's order: `sys_sched_setscheduler()` rejects `policy < 0`; `do_sched_setscheduler()` rejects a negative `pid` or NULL `param`, copies `param` in (EFAULT) and looks the target up with `find_process_by_pid()` (ESRCH, answered here by `thread_is_live`, `detcore/src/syscalls/threads.rs:2423`); `__sched_setscheduler()` then checks policy and priority (EINVAL) before `user_check_sched_setscheduler()` (EPERM). On success the call writes the target thread's virtual scheduling record, the per-thread record kept in Detcore's thread state next to `timer_slack_ns` (`detcore/src/tool_local.rs:1795`): policy, `rt_priority` and the reset-on-fork flag, keeping nice (F3, F4, F6). `sched_getscheduler`, `sched_getparam`, `sched_getattr` and `sched_rr_get_interval` report the record. A fork or clone child copies it (as the timer slack is copied at `detcore/src/lib.rs:1555`); when the reset-on-fork flag is set, a real-time child gets `SCHED_OTHER` priority 0, a negative nice becomes 0, and the child's flag is clear (kernel `sched_fork()`). `execve` keeps the record. Nothing reaches the host or Detcore's scheduler. Timer slack follows Linux's `__setscheduler_params()`: recording a real-time policy sets the thread's virtual slack to 0, returning to a fair policy restores the default slack, and PR_SET_TIMERSLACK is ignored while the virtual policy is real-time, as the prctl body's F6 describes. The comment at `detcore/src/syscalls/misc.rs:384-385` and the assertions from `detcore/tests/misc/mod.rs:315` onwards change accordingly. The EPERM cases in F4, F6 and F8 stay accepted under the privilege rule (`detcore/src/syscalls/threads.rs:2294`). Today the arm returns 0 and stores nothing (`detcore/src/lib.rs:2652`).

## Recommended tests

- T1 (covers F3, F4, F7): table test under `hermit run --verify` on ptrace that calls
  `syscall(SYS_sched_setscheduler, 0, policy, &param)` for policies 0, 1, 2, 3, 5 with legal and
  illegal priorities (0, 1, 99, 100, and 5 for the non-real-time policies), plus policy 4, policy 8 and
  policy -1. Expect 0 for the legal pairs and EINVAL for the rest. The real-time cases use priority 1,
  which Hermit accepts as a no-op whatever the caller's RLIMIT_RTPRIO. Existing candidate: the
  SCHED_FIFO step in detcore/tests/misc/mod.rs:313-315 (`timer_slack_prctl_and_procfs_share_virtual_state`)
  already asserts that one legal real-time request returns 0.
- T2 (covers F1, F2): pid and pointer order. `pid = -1` and a NULL `param` give EINVAL; a non-NULL
  unmapped `param` gives EFAULT; a thread id that never existed gives ESRCH; the tid of a live non-leader
  thread from `gettid()` gives 0. Also a two-errors case: a nonexistent pid with policy 4 must give ESRCH,
  not EINVAL, because the pid lookup comes before the policy check.
- T3 (covers F5): SCHED_DEADLINE (6) through `sched_setscheduler` gives EINVAL on every host.
- T4 (covers F6): `SCHED_OTHER | SCHED_RESET_ON_FORK` with priority 0 returns 0, and
  `SCHED_FIFO | SCHED_RESET_ON_FORK` with priority 1 returns 0. The run passes `--verify`, so stdout is
  identical across runs.
- T5 (covers F4, F8): the privilege-dependent requests: SCHED_FIFO priority 99 as an unprivileged user,
  and a SCHED_IDLE to SCHED_OTHER transition. Hermit must return the same answer (0) on every host and
  under `--verify`. Do not compare these with the host kernel's answer, for the reason given in the
  "Deliberately not emulated" block of `handle_sched_setattr`.
- T6 (covers F3, F4, F6): read-back under `hermit run --verify` on ptrace. After `SCHED_FIFO` priority 10,
  `sched_getscheduler(0)` reports `SCHED_FIFO`, `sched_getparam` reports 10 and PR_GET_TIMERSLACK reports
  0; PR_SET_TIMERSLACK leaves it 0. After `SCHED_RR | SCHED_RESET_ON_FORK` priority 5, `sched_getscheduler`
  reports `SCHED_RR | SCHED_RESET_ON_FORK`, and a forked child reports `SCHED_OTHER`, priority 0, no flag.
  After `setpriority` to nice 5 and then `SCHED_BATCH` priority 0, `getpriority` still reports 5 and
  PR_GET_TIMERSLACK reports the default again. A sibling thread is unaffected throughout. Today Hermit
  reports `SCHED_OTHER` and priority 0 after every call.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant A1 of https://github.com/rrnewton/hermit/issues/419: linux_determinism conditional (from **Classification:**); target_policy from legacy NEEDS-HANDLER; an agent chooses model, emulate or refuse; code at this SHA: determinized, dispatch typed.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: target_policy investigate -> emulate (suppress, add Linux argument validation in kernel order as sched_setattr does; the lib.rs:2652 arm returns 0 for invalid arguments today); linux_determinism conditional kept; 8 flag behaviours and 5 tests; left todo with an owner question on modelling a read-back policy.
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d615 (owner decisions 6-15): decisions 7 and 6 applied (per-thread policy, priority and reset-on-fork record with read-back, fork inheritance and the virtual timer-slack rule, no host effect; Linux checks in kernel order); target_policy emulate -> model; new T6; removed the open question on modelling the read-back

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `sched_setscheduler`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+sched_setscheduler&body=Discussion+for+the+%60sched_setscheduler%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fsched_setscheduler.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60sched_setscheduler%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: sched_setscheduler" (https://github.com/rrnewton/hermit/issues/419), legacy variant A1.

### 2026-07-24 host strace evidence

Evidence gathered on this host (Linux 6.17 x86_64, glibc) with `strace -f -e trace=sched_setscheduler` on a direct `syscall()` harness, invoked with arguments that query, do nothing, or fail, and on common programs where applicable. At that time the call had not yet been classified. At hermit f4055eb9cae4 it is determinized (detcore/src/syscall_classification.rs:334).

Observed strace line:
```
sched_setscheduler(0, SCHED_OTHER, [0]) = 0
```

---
*Autonomous syscall-classification sweep of the calls that were then awaiting classification in `syscall_classification.rs`. Host testing + analysis only; no code change proposed here.*
