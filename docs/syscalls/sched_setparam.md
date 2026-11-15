---
schema: syscall-pages/v1
syscall: sched_setparam
nr_x86_64: 142
linux_determinism: conditional
target_policy: model
tracking_status: todo
cross_host_variant: false
related:
  - https://github.com/rrnewton/hermit/issues/418
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+sched_setparam&body=Discussion+for+the+%60sched_setparam%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fsched_setparam.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60sched_setparam%60%2C+or+in+what+the+page+says%3F%0A
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

`sched_setparam(pid, param)` sets a thread's static scheduling priority (`sched_priority`) without changing its scheduling policy. Detcore replaces the Linux scheduler, so the change can have no effect, and the typed arm returns 0 for every call without reading its arguments (`detcore/src/lib.rs:2653`). This body keeps that suppressed effect, which is an `emulate` policy, and proposes the argument checks Linux makes, as `sched_setattr`'s handler already does.

Manual reference: [sched_setparam(2)](https://man7.org/linux/man-pages/man2/sched_setparam.2.html).

## Linux semantics

The kernel handles the call in this order:

1. A NULL `param` or a negative `pid` is `EINVAL`, before any memory access.
2. The `struct sched_param` is copied in. An unreadable pointer is `EFAULT`.
3. The target is looked up. `pid == 0` is the calling thread, and any other value can be any live thread ID, not only a thread-group leader. No match is `ESRCH`.
4. The target's current policy is kept, and the new priority is checked against it. `SCHED_FIFO` and `SCHED_RR` need a priority from 1 to 99. Every other policy needs exactly 0. Anything else is `EINVAL`.
5. Permission is checked. Without `CAP_SYS_NICE`, the caller's effective uid must match the target's real or effective uid, and a real-time priority can be raised only up to `RLIMIT_RTPRIO`. A refusal is `EPERM`. A Linux security module can also refuse with `EPERM`.

For a thread running `SCHED_OTHER`, which is the normal case, the only valid request is priority 0. It succeeds and changes nothing. The 2026-07-24 host trace under Additional information shows this case: `sched_setparam(0, [0]) = 0`.

### Flags and options

- F1: `pid == 0`: the calling thread
- F2: `pid > 0`: any live thread ID, including a thread that is not a thread-group leader
- F3: `pid < 0` or a NULL `param`: `EINVAL`, before any memory access
- F4: `param` is unreadable: `EFAULT`, before the target is looked up
- F5: no task has ID `pid`: `ESRCH`
- F6: the target's policy is not real-time: `sched_priority` must be 0, otherwise `EINVAL`
- F7: the target's policy is `SCHED_FIFO` or `SCHED_RR`: `sched_priority` must be 1 to 99, otherwise `EINVAL`
- F8: `EPERM` when the caller does not own the target, or raises a real-time priority above `RLIMIT_RTPRIO`, and lacks `CAP_SYS_NICE`
- F9: effect: the new priority changes the target's position on the host's run queue and is what `sched_getparam` and `sched_getattr`'s `sched_priority` report afterwards

## Determinism analysis

`linux_determinism: conditional`, kept from the legacy classification. Nothing in the call races. Its result depends on inputs from outside the program:

- the policy the target is running when the call is made, which the program inherits from whoever launched it (a launcher such as `chrt -f` makes priority 1 valid and priority 0 invalid; F6, F7);
- the caller's capabilities and `RLIMIT_RTPRIO` (F8);
- whether a thread with ID `pid` exists (F5).

A successful change on a real-time thread also changes the order in which the host runs threads. That would perturb timing, but Detcore runs guest threads one at a time in its own order, so the change must not reach the host at all.

### Nondeterminism sources

- The target's current policy, inherited from the launcher, which decides which priorities are valid
- Capabilities and `RLIMIT_RTPRIO`, which decide `EPERM`
- Whether the target thread exists
- A real priority change would alter host scheduling of guest threads

## Hermit design

Target policy: `model` (owner decision 7, 2026-10-04). The p3 plan in the next paragraphs was `emulate` against a fixed policy; the Target paragraph at the end of this section replaces it. Today Detcore presents one scheduling policy, `SCHED_OTHER`, for every thread: `sched_getscheduler` returns 0 (`detcore/src/lib.rs:2655`), `sched_getparam` writes priority 0 (`detcore/src/syscalls/threads.rs:2189`), and `sched_setattr` treats `SCHED_OTHER` as the current policy (`VIRTUAL_CURRENT_POLICY`, `detcore/src/syscalls/threads.rs:112`). Against that fixed virtual state, `sched_setparam` should check its arguments the way Linux does and never touch the host:

- `EINVAL` for a negative `pid` or NULL `param` (F3), as `handle_sched_setattr` does (`detcore/src/syscalls/threads.rs:2327`).
- `EFAULT` when `param` cannot be read (F4). The backend reports a failed read as `EIO`, so the handler maps it, as `handle_sched_setattr` explains (`detcore/src/syscalls/threads.rs:2336`).
- `ESRCH` when `pid` is not a live guest thread, answered from Detcore's task table with `thread_is_live` (`detcore/src/syscalls/threads.rs:2423`), which accepts any live thread and not only leaders (F2, F5).
- `EINVAL` when `sched_priority` is not 0, because the virtual policy is never real-time (F6). This is the same rule `sched_setattr` applies when `SCHED_FLAG_KEEP_POLICY` keeps the virtual policy (`detcore/src/syscalls/threads.rs:359`, `detcore/src/syscalls/threads.rs:377`).
- Return 0 otherwise, with no effect (F1, F9).
- F7 cannot occur, because no guest thread is ever reported as real-time.
- F8 is never reported. This follows the rule `handle_sched_setattr` states (`detcore/src/syscalls/threads.rs:2294`): an answer that depends on the caller's privileges is not reproduced. Its first example is `EPERM` for a real-time priority (`detcore/src/syscalls/threads.rs:2300`).

The code at hermit f4055eb9cae4 realizes the suppressed effect but not the checks. The BATCH 3 comment above the arm says setters for the scheduling calls are no-ops (`detcore/src/lib.rs:2638`), and `sched_setparam` is in the Determinized list (`detcore/src/syscall_classification.rs:335`). The arm returns `Ok(0)` (`detcore/src/lib.rs:2653`), so F3, F4 and F5 succeed where Linux fails, and a non-zero priority is accepted where Linux, for a `SCHED_OTHER` thread, gives `EINVAL`. `handle_sched_setattr`'s doc says it matches "the existing sched_setscheduler and sched_setparam policy" (`detcore/src/syscalls/threads.rs:2261`). That holds for suppressing the effect, but not for checking arguments, which `sched_setattr` does and `sched_setparam` does not.

Why the p3 plan chose `emulate`: it treated the guest-visible state as a constant policy and priority. Decision 7 makes that state per-thread and settable, so the target is now `model`. `passthrough` would let a privileged guest change real host priorities, and would make `EPERM` and `EINVAL` depend on the launcher. `refuse` would break programs that set priority 0 or probe the call. Adding the checks needs no new state: every check reads either the guest's arguments or Detcore's task table.

Target (owner decision 7, with decision 6): every guest thread carries a virtual scheduling record in Detcore's per-thread state: policy, `rt_priority`, the `SCHED_RESET_ON_FORK` flag, nice, the deadline parameters and the utilization clamps. It sits next to the virtual timer slack (`detcore/src/tool_local.rs:1795`), is copied into each new thread as the timer slack is (`detcore/src/lib.rs:1555`), and starts as `SCHED_OTHER`, priority 0, nice 0. `sched_setparam` keeps the recorded policy and stores the new priority; nothing reaches the host or Detcore's scheduler. The checks follow the kernel's order (sched_setparam(2); kernel `do_sched_setparam()`, then `__sched_setscheduler()`): `EINVAL` for a negative `pid` or a NULL `param` (F3); `EFAULT` when the copy-in fails (F4), mapping the backend's `EIO` as `handle_sched_setattr` does (`detcore/src/syscalls/threads.rs:2336`); `ESRCH` from `thread_is_live` (`detcore/src/syscalls/threads.rs:2423`) (F2, F5); then `EINVAL` when the priority does not fit the recorded policy, 0 for a policy that is not real-time (F6) and 1 to 99 for `SCHED_FIFO` or `SCHED_RR` (F7). F7 becomes reachable once `sched_setscheduler` or `sched_setattr` records a real-time policy, so `sched_setparam(0, {1})` after an accepted `sched_setscheduler(0, SCHED_FIFO, {1})` succeeds and `sched_getparam` reports 1 (F9, T5). F8 stays unreported, under the privilege rule at `detcore/src/syscalls/threads.rs:2294`. Today the arm returns 0 without reading anything (`detcore/src/lib.rs:2653`).

## Recommended tests

- T1 (covers F1, F2, F6, F9): under `hermit run --verify` on ptrace, `sched_setparam(0, {0})` returns 0, and so does the same call on a forked child's PID and on a second thread's TID. Afterwards `sched_getparam` and `sched_getscheduler` still report priority 0 and `SCHED_OTHER`.
  Existing coverage: none on ptrace. The rr program `sched_setparam` is wired as `rr_sched_setparam` (`hermit-cli/tests/rr_suite.rs:412`), but every `rr_test!` is `#[ignore]` (`hermit-cli/tests/rr_suite.rs:233`) and builds from an external rr tree, so it gives no ptrace coverage in CI.
- T2 (covers F3, F4, F5): `pid = -1` gives `EINVAL`, a NULL `param` gives `EINVAL`, a `param` pointing at an unmapped page gives `EFAULT`, and a PID with no guest thread gives `ESRCH`. Today Hermit returns 0 for all four.
- T3 (covers F6, F7): `sched_setparam(0, {1})` and `sched_setparam(0, {100})` give `EINVAL`, because the virtual policy is `SCHED_OTHER`. Run once with Hermit launched under host `chrt -f 10`, to show that the launcher's real-time policy does not reach the guest and priority 1 is still refused.
- T4 (covers F8): run the guest with the host's `RLIMIT_RTPRIO` set to 0, and with a child that has switched to uid 1000 targeting its uid-0 parent with priority 0. On Linux the second gives `EPERM`; record what Hermit returns, which under this policy is 0.
- T5 (covers F7, F9): after `sched_setscheduler(0, SCHED_FIFO, {10})` returns 0, `sched_setparam(0, {20})` returns 0 and `sched_getparam` reports 20; `sched_setparam(0, {0})` and `sched_setparam(0, {100})` give `EINVAL`; a sibling thread that never changed its policy still reports priority 0. Output is identical across both runs of `hermit run --verify` on ptrace. Today Hermit reports 0 after every call.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant A1 of https://github.com/rrnewton/hermit/issues/418: linux_determinism conditional (from **Classification:**); target_policy from legacy NEEDS-HANDLER; an agent chooses model, emulate or refuse; code at this SHA: determinized, dispatch typed.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: linux_determinism stays conditional; target_policy investigate -> emulate (fixed SCHED_OTHER state, effect suppressed); the code suppresses the effect but skips Linux's EINVAL/EFAULT/ESRCH checks and accepts non-zero priorities, so it does not yet realize the policy; open question on non-zero priority after an accepted sched_setscheduler(SCHED_FIFO); 9 flag behaviours and 4 tests
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d615 (owner decisions 6-15): decisions 7 and 6 applied (per-thread scheduling record with no host effect; Linux checks in kernel order, priority checked against the recorded policy); target_policy emulate -> model; new T5; removed the open question on priority 1 after an accepted SCHED_FIFO (decision 7: accept and report it)

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `sched_setparam`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+sched_setparam&body=Discussion+for+the+%60sched_setparam%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fsched_setparam.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60sched_setparam%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: sched_setparam" (https://github.com/rrnewton/hermit/issues/418), legacy variant A1.

### 2026-07-24 host strace evidence

Evidence gathered on this host (Linux 6.17 x86_64, glibc) with `strace -f -e trace=sched_setparam` on a direct `syscall()` harness (invoked with non-destructive query/no-op/failing arguments) and, where applicable, on common programs. Hermit then routed `sched_setparam` through the UNCLASSIFIED temporary-passthrough block in `detcore/src/syscall_classification.rs`. (That was the state on 2026-07-24; at f4055eb9cae4 the call is Determinized and handled as described above.)

Observed strace line:
```
sched_setparam(0, [0])          = 0
```

---
*Autonomous syscall-classification sweep (ranks 151-250 of the `syscall_classification.rs` UNCLASSIFIED list). Host testing + analysis only; no code change proposed here.*
