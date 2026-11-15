---
schema: syscall-pages/v1
syscall: sched_getparam
nr_x86_64: 143
linux_determinism: conditional
target_policy: model
tracking_status: todo
cross_host_variant: false
related:
  - https://github.com/rrnewton/hermit/issues/414
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+sched_getparam&body=Discussion+for+the+%60sched_getparam%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fsched_getparam.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60sched_getparam%60%2C+or+in+what+the+page+says%3F%0A
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

`sched_getparam(pid, param)` reads a thread's static scheduling priority into a `struct sched_param`. Detcore answers from its fixed virtual scheduling state without asking the host: `handle_sched_getparam` writes priority 0 and returns 0 (`detcore/src/syscalls/threads.rs:2183`). That is an `emulate` policy. This body keeps it and proposes adding the argument checks Linux makes, which the handler skips today.

Manual reference: [sched_getparam(2)](https://man7.org/linux/man-pages/man2/sched_setparam.2.html) (shared page with `sched_setparam`).

## Linux semantics

The kernel handles the call in this order:

1. A NULL `param` or a negative `pid` is `EINVAL`.
2. The target is looked up. `pid == 0` is the calling thread, and any other value can be any live thread ID, not only a thread-group leader. No match is `ESRCH`.
3. A Linux security module may refuse with `EPERM`. Without one, any caller may read any thread's priority.
4. The priority is `rt_priority` for a thread running `SCHED_FIFO` or `SCHED_RR`, from 1 to 99, and 0 for every other policy.
5. The result is copied out. An unwritable `param` is `EFAULT`, found only after the lookup, so a missing target with a bad pointer is `ESRCH`.

The 2026-07-24 host trace under Additional information shows the common case: `sched_getparam(0, [0]) = 0`.

### Flags and options

- F1: `pid == 0`: the calling thread
- F2: `pid > 0`: any live thread ID, including a thread that is not a thread-group leader
- F3: `pid < 0` or a NULL `param`: `EINVAL`
- F4: no task has ID `pid`: `ESRCH`
- F5: the target runs `SCHED_FIFO` or `SCHED_RR`: its real-time priority, 1 to 99
- F6: the target runs any other policy: 0
- F7: `param` is unwritable: `EFAULT`, after the target lookup

## Determinism analysis

`linux_determinism: conditional`, kept from the legacy classification. The answer is the target's current priority, which is fixed unless something outside the program changes it:

- the policy and priority the program inherited from its launcher (a launcher such as `chrt -f 10` makes the answer 10; F5);
- other processes changing the thread's priority, for example a host `chrt -p`;
- whether a thread with ID `pid` exists (F4).

For a program started normally and not changed from outside, every thread reports 0, so the call is deterministic in practice. The conditions above are what make it `conditional` and not `deterministic`.

### Nondeterminism sources

- Scheduling policy and priority inherited from the launcher
- Changes made by processes outside the program
- Whether the target thread exists

## Hermit design

Target policy: `model` (owner decision 7, 2026-10-04). The p3 plan in the next paragraphs was `emulate` against a fixed policy; the Target paragraph at the end of this section replaces it. Today Detcore presents one scheduling policy, `SCHED_OTHER`, for every thread (`VIRTUAL_CURRENT_POLICY`, `detcore/src/syscalls/threads.rs:112`), and `sched_setparam`, `sched_setscheduler` and `sched_setattr` never change it. So the priority is always 0, and `sched_getparam` should report it after Linux's argument checks:

- `EINVAL` for a negative `pid` or NULL `param` (F3), as `handle_sched_setattr` does for the same clause (`detcore/src/syscalls/threads.rs:2327`).
- `ESRCH` when `pid` is not a live guest thread, answered from Detcore's task table with `thread_is_live` (`detcore/src/syscalls/threads.rs:2423`), which accepts any live thread and not only leaders (F2, F4).
- Write `sched_priority = 0` and return 0 (F1, F6).
- `EFAULT` when the write fails (F7).
- F5 cannot occur, because no guest thread is ever real-time under Detcore, whatever the launcher's policy was.

The code at hermit f4055eb9cae4 realizes the value but not the checks. The typed arm (`detcore/src/lib.rs:2656`) calls `handle_sched_getparam`, which writes `sched_param { sched_priority: 0 }` only if `param` is not NULL (`detcore/src/syscalls/threads.rs:2188` to `detcore/src/syscalls/threads.rs:2190`) and returns 0. It never looks at `pid`. So:

- F1, F2 and F6 match Linux for a normally started program.
- F3 differs: a NULL `param` returns 0 without writing, and a negative `pid` writes 0 and returns 0. Linux gives `EINVAL` for both.
- F4 differs: a PID with no guest thread returns 0 instead of `ESRCH`.
- F7: a failed write propagates whatever error the memory layer reports. `handle_sched_setattr` notes that the backend reports a failed read as `EIO` and maps it to `EFAULT` (`detcore/src/syscalls/threads.rs:2336`). Whether a failed write is also `EIO` was not checked; if it is, `sched_getparam` returns an errno Linux never gives here.

Why the p3 plan chose `emulate`: it treated the guest-visible state as a constant, the same constant `sched_getscheduler` (`detcore/src/lib.rs:2655`) and `sched_getattr` (`detcore/src/syscalls/threads.rs:2218`) report, so there is nothing to model. `passthrough` would report the launcher's real priority and break `--verify` across launchers. Adding the checks needs no new state.

Target (owner decision 7, with decision 6): `sched_getparam` reports `rt_priority` from the target thread's virtual scheduling record, the per-thread record that `sched_setscheduler`, `sched_setparam` and `sched_setattr` write (kept in Detcore's thread state next to `timer_slack_ns`, `detcore/src/tool_local.rs:1795`, and copied at clone as at `detcore/src/lib.rs:1555`). The record starts as `SCHED_OTHER` priority 0, whatever the launcher's real policy, so T3 still holds; after a guest thread records `SCHED_FIFO` or `SCHED_RR`, its priority is reported (F5). A fork child inherits the parent's record, except that `SCHED_RESET_ON_FORK` resets a real-time policy to `SCHED_OTHER` priority 0 (kernel `sched_fork()`). The checks follow the kernel's order (sched_getparam(2); kernel `sys_sched_getparam()`): `EINVAL` for a negative `pid` or NULL `param` (F3), `ESRCH` from `thread_is_live` (`detcore/src/syscalls/threads.rs:2423`) (F2, F4), then the copy-out, whose failure is `EFAULT` (F7). Today the handler writes 0 without reading `pid` (`detcore/src/syscalls/threads.rs:2188` to `detcore/src/syscalls/threads.rs:2190`).

## Recommended tests

- T1 (covers F1, F2, F6): under `hermit run --verify` on ptrace, `sched_getparam` with `pid = 0`, with a forked child's PID and with a second thread's TID each returns 0 and writes priority 0.
  Existing coverage: none on ptrace. The compat corpus row `chrt` (`tests/e2e/manifests/compat.yaml:185`) runs `chrt -p $$` on ptrace, but `handle_sched_getattr`'s doc says it is what makes `chrt` work (`detcore/src/syscalls/threads.rs:2220`), so that row probably reaches `sched_getattr` and not `sched_getparam`; this was not traced.
- T2 (covers F3, F4): `pid = -1` gives `EINVAL`, a NULL `param` gives `EINVAL`, and a PID with no guest thread gives `ESRCH`. Today Hermit returns 0 for all three.
- T3 (covers F5): launch Hermit under host `chrt -f 10`; the guest still reads priority 0 and `sched_getscheduler` still reports `SCHED_OTHER`, so the launcher's real-time policy does not reach the guest.
- T4 (covers F7): `param` pointing at an unmapped page gives `EFAULT`, and `param` pointing at an unmapped page with a PID that does not exist gives `ESRCH`, showing the lookup comes first.
- T5 (covers F2, F5): thread A calls `sched_setscheduler(0, SCHED_FIFO, {10})`; thread B's `sched_getparam(A's TID)` reports 10, and its own `sched_getparam(0)` reports 0. A child forked by A reports 10; a child forked after `sched_setscheduler(0, SCHED_FIFO | SCHED_RESET_ON_FORK, {10})` reports 0. Output is identical across both runs of `hermit run --verify` on ptrace. Today Hermit reports 0 in every case.

## Done criteria

`done` means (owner rule, 2026-10-02): every flag and option listed under `### Flags and options` is covered by a named test, recorded in Status as `T<n> covered by <path>`, or, for an error only the host's configuration can produce, by a `host-config` U item that cites where Hermit forwards the error unchanged (owner decision 1, 2026-10-04); a `host-transient` U item does not count until Hermit hides that error or reports it as an infrastructure error; each such test exists at the cited hermit SHA and runs in CI on the default backend in the strict container; the code realizes `target_policy`; and the Status log records that evidence. If a problem is later found with this syscall, `tracking_status` returns to `todo` or `in-progress` with a Status entry saying why.

## Status

- 2026-10-03 · hermit fe5d48903806 · syscall-tracking · migrated to v1 from legacy variant A1 of https://github.com/rrnewton/hermit/issues/414: linux_determinism conditional (from **Classification:**); target_policy from legacy NEEDS-HANDLER; an agent chooses model, emulate or refuse; code at this SHA: determinized, dispatch typed.
- 2026-10-03 · hermit f4055eb9cae4 · syscall-tracking-p3 · p3: linux_determinism stays conditional; target_policy investigate -> emulate (constant SCHED_OTHER priority 0, which the handler already writes); the handler skips Linux's EINVAL and ESRCH checks and its EFAULT errno is unverified, so it does not yet fully realize the policy; 7 flag behaviours and 4 tests
- 2026-10-04 · hermit f4055eb9cae4 · syscall-tracking · p3-d615 (owner decisions 6-15): decisions 7 and 6 applied (report rt_priority from the per-thread scheduling record, inherited at fork with SCHED_RESET_ON_FORK; Linux checks in kernel order); target_policy emulate -> model; new T5; no open questions

- 2026-11-15 · hermit 239ca43d3092 · syscall-pages · converted: moved from the syscall-tracking/v1 issue body to this page; code facts refreshed at this SHA; per-backend verification fields are not carried in page form.

## Discussion

[Start a discussion about `sched_getparam`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+sched_getparam&body=Discussion+for+the+%60sched_getparam%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fsched_getparam.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60sched_getparam%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Additional information

Originally filed 2026-07-24 as "Syscall classification: sched_getparam" (https://github.com/rrnewton/hermit/issues/414), legacy variant A1. The handler carries `TODO-HUMAN-REVIEW(#720)` (`detcore/src/syscalls/threads.rs:2182`), from https://github.com/rrnewton/hermit/pull/720, which determinized the NUMA and CPU-scheduling calls.

### 2026-07-24 host strace evidence

Evidence gathered on this host (Linux 6.17 x86_64, glibc) with `strace -f -e trace=sched_getparam` on a direct `syscall()` harness (invoked with non-destructive query/no-op/failing arguments) and, where applicable, on common programs. Hermit then routed `sched_getparam` through the UNCLASSIFIED temporary-passthrough block in `detcore/src/syscall_classification.rs`. (That was the state on 2026-07-24; at f4055eb9cae4 the call is Determinized and handled as described above.)

Observed strace line:
```
sched_getparam(0, [0])          = 0
```

---
*Autonomous syscall-classification sweep (ranks 151-250 of the `syscall_classification.rs` UNCLASSIFIED list). Host testing + analysis only; no code change proposed here.*
